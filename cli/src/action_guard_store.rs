// SPDX-FileCopyrightText: 2026 The Panio Authors
// SPDX-License-Identifier: Apache-2.0

//! Durable receipts, power reservations, and power mutation records.
//! Changes and hardware checks run under the caller's real target hook lock.

use std::fs;
use std::io::{self, Write};
use std::path::PathBuf;

use anyhow::{Context, Result};
use serde::{de::DeserializeOwned, Deserialize, Serialize};

use crate::{error, model, platform, ssh, state};

const RECEIPT: &str = "action-guard.json";
const RESERVATION: &str = "power-reservation.json";
const MUTATION: &str = "power-mutation.json";
const MUTATION_LOCK: &str = "power-mutation.lock";

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Phase {
    Ready,
    Running,
    Finished,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Participant {
    pub host: String,
    pub identity: String,
    pub generation: String,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Ticket {
    pub id: String,
    pub action: String,
    pub required: String,
    pub hosts: Vec<Participant>,
    pub phase: Phase,
}

impl Ticket {
    fn valid(&self) -> bool {
        valid_id(&self.id)
            && !self.action.trim().is_empty()
            && model::VALID_REQUIRES_POWER.contains(&self.required.as_str())
            && !self.hosts.is_empty()
            && self.hosts.iter().enumerate().all(|(index, participant)| {
                !participant.host.trim().is_empty()
                    && valid_id(&participant.identity)
                    && valid_id(&participant.generation)
                    && !self.hosts[..index]
                        .iter()
                        .any(|prior| prior.host == participant.host)
            })
    }
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct PowerState {
    identity: String,
    generation: String,
    // An explicit null means available. Missing its holder field is corruption.
    #[serde(deserialize_with = "Option::deserialize")]
    holder: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct PowerMutation {
    pub id: String,
    pub operation: String,
    pub pending_children: u32,
}

pub struct Store {
    target: String,
    host: String,
    dir: PathBuf,
    #[cfg(unix)]
    home: PathBuf,
}

impl Store {
    pub fn for_target(target: &str) -> Self {
        Self::for_target_on_host(target, "local")
    }

    pub fn for_target_on_host(target: &str, host: &str) -> Self {
        Self {
            target: target.to_string(),
            host: host.to_string(),
            dir: state::target_dir(target),
            #[cfg(unix)]
            home: dirs::home_dir().unwrap_or_else(|| PathBuf::from(".")),
        }
    }

    #[cfg(test)]
    pub fn at(target: &str, dir: PathBuf) -> Self {
        Self {
            target: target.to_string(),
            host: "local".to_string(),
            #[cfg(unix)]
            home: dir.parent().unwrap_or(&dir).to_path_buf(),
            dir,
        }
    }

    pub fn begin(&self, action: &str, required: &str, hosts: &[Participant]) -> Result<Ticket> {
        if let Some(ticket) = self.ticket()? {
            return Err(self.refusal(
                &ticket.id,
                format!("action '{}' still has a receipt", ticket.action),
            ));
        }
        let ticket = Ticket {
            id: platform::random_token().context("minting an action guard id")?,
            action: action.to_string(),
            required: required.to_string(),
            hosts: hosts.to_vec(),
            phase: Phase::Ready,
        };
        anyhow::ensure!(ticket.valid(), "invalid action guard receipt");
        self.write(RECEIPT, &ticket)
            .with_context(|| self.recovery(&ticket.id))?;
        Ok(ticket)
    }

    pub fn ticket(&self) -> Result<Option<Ticket>> {
        let ticket = self.read::<Ticket>(RECEIPT)?;
        if let Some(ticket) = &ticket {
            anyhow::ensure!(
                ticket.valid(),
                "invalid action guard receipt for '{}': {RECEIPT}",
                self.target
            );
        }
        Ok(ticket)
    }

    /// Record Running before the caller can launch the action hook.
    pub fn start(&self, id: &str) -> Result<Ticket> {
        let mut ticket = self.matching_ticket(id)?;
        if ticket.phase != Phase::Ready {
            return Err(self.refusal(id, "the action receipt is no longer ready"));
        }
        self.set_phase(&mut ticket, Phase::Running)?;
        Ok(ticket)
    }

    pub fn finish(&self, id: &str) -> Result<()> {
        let mut ticket = self.matching_ticket(id)?;
        if ticket.phase != Phase::Running {
            return Err(self.refusal(id, "the action receipt is no longer running"));
        }
        self.set_phase(&mut ticket, Phase::Finished)
    }

    /// Fence any delayed execution before releasing power. A Running receipt
    /// can outlive the parent and its lock while a hook subprocess continues.
    pub fn seal(&self, id: &str, confirm_stopped: bool) -> Result<Ticket> {
        let mut ticket = self.matching_ticket(id)?;
        if ticket.phase == Phase::Running && !confirm_stopped {
            return Err(self.refusal(
                id,
                "the action may still be running. Verify the hook has stopped before using --confirm-stopped",
            ));
        }
        if ticket.phase == Phase::Finished {
            // A prior finish may have replaced the file but failed to sync
            // the directory. Confirm durability before releasing power.
            self.sync_record(RECEIPT)
                .with_context(|| self.recovery(id))?;
        } else {
            self.set_phase(&mut ticket, Phase::Finished)?;
        }
        Ok(ticket)
    }

    pub fn forget(&self, id: &str) -> Result<()> {
        let ticket = self.matching_ticket(id)?;
        if ticket.phase != Phase::Finished {
            return Err(self.refusal(id, "the action receipt has not been sealed"));
        }
        self.remove(RECEIPT).with_context(|| self.recovery(id))
    }

    /// Return the identity and current generation before opening a receipt.
    /// These bind later requests to this store and the reservation epoch they saw.
    pub fn participant(&self, host: &str) -> Result<Participant> {
        self.require_no_mutation(host)?;
        let power = match self.power_state()? {
            Some(power) => {
                self.sync_record(RESERVATION)?;
                power
            }
            None => {
                let power = PowerState {
                    identity: platform::random_token()
                        .context("minting an action host identity")?,
                    generation: new_generation()?,
                    holder: None,
                };
                self.write(RESERVATION, &power)?;
                power
            }
        };
        Ok(Participant {
            host: host.to_string(),
            identity: power.identity,
            generation: power.generation,
        })
    }

    pub fn reserve(&self, id: &str, peer: &Participant) -> Result<()> {
        // A power hook can inherit the hardware lock. Serialize reservation
        // creation with begin_mutation even when that caller borrowed it.
        let _lock = self.lock_mutation()?;
        self.require_no_mutation(&peer.host)?;
        let mut power = self.current_power(id, peer)?;
        match power.holder.as_deref() {
            Some(holder) if holder == id => self
                .sync_record(RESERVATION)
                .with_context(|| self.recovery(id)),
            Some(holder) => Err(self.refusal(holder, "power is reserved")),
            None => {
                power.holder = Some(id.to_string());
                self.write(RESERVATION, &power)
                    .with_context(|| self.recovery(id))
            }
        }
    }

    pub fn assert_reserved(&self, id: &str, peer: &Participant) -> Result<()> {
        self.require_no_mutation(&peer.host)?;
        let power = self.current_power(id, peer)?;
        match power.holder.as_deref() {
            Some(holder) if holder == id => self
                .sync_record(RESERVATION)
                .with_context(|| self.recovery(id)),
            Some(holder) => Err(self.refusal(holder, "power is reserved")),
            None => Err(self.refusal(id, "the expected power reservation is missing")),
        }
    }

    pub fn power_available(&self) -> Result<()> {
        self.require_no_mutation(&self.host)?;
        match self.power_state()?.and_then(|power| power.holder) {
            Some(holder) => Err(self.refusal(&holder, "power is reserved")),
            None => Ok(()),
        }
    }

    /// Record the mutation before any operation that can change power.
    pub fn begin_mutation(&self, operation: &str) -> Result<String> {
        let _lock = self.lock_mutation()?;
        self.power_available()?;
        anyhow::ensure!(
            valid_operation(operation),
            "invalid power mutation operation"
        );
        let marker = PowerMutation {
            id: platform::random_token().context("minting a power mutation id")?,
            operation: operation.to_string(),
            pending_children: 0,
        };
        self.write(MUTATION, &marker)
            .with_context(|| self.mutation_recovery(&self.host, &marker.id))?;
        Ok(marker.id)
    }

    /// Clear only the caller's marker after its power operation has stopped.
    pub fn finish_mutation(&self, id: &str) -> Result<()> {
        self.validate_id(id)?;
        let _lock = self.lock_mutation()?;
        if let Some(marker) = self.mutation()? {
            if marker.id != id || marker.pending_children != 0 {
                return Err(self.mutation_refusal(&self.host, &marker));
            }
        }
        self.remove(MUTATION)
            .with_context(|| self.mutation_recovery(&self.host, id))
    }

    /// Inspect a durable marker without acquiring the hardware lock.
    pub fn mutation(&self) -> Result<Option<PowerMutation>> {
        self.read_mutation(&self.host)
    }

    /// Explicit recovery of an interrupted mutation under the real hook lock.
    pub fn resume_mutation(&self, id: &str, confirm_stopped: bool) -> Result<()> {
        let _lock = self.lock_mutation()?;
        let marker = self.matching_mutation(id)?;
        if !confirm_stopped {
            return Err(self.mutation_refusal(&self.host, &marker));
        }
        self.remove(MUTATION)
            .with_context(|| self.mutation_recovery(&self.host, id))
    }

    /// Register a descendant using the enclosing power operation's marker.
    pub fn begin_nested_mutation(&self, id: &str) -> Result<()> {
        let _lock = self.lock_mutation()?;
        let mut marker = self.matching_mutation(id)?;
        marker.pending_children = marker
            .pending_children
            .checked_add(1)
            .context("too many pending nested power mutations")?;
        self.write(MUTATION, &marker)
            .with_context(|| self.mutation_recovery(&self.host, id))
    }

    /// A descendant calls this once after it knows its operation stopped.
    pub fn finish_nested_mutation(&self, id: &str) -> Result<()> {
        let _lock = self.lock_mutation()?;
        let mut marker = self.matching_mutation(id)?;
        marker.pending_children = marker
            .pending_children
            .checked_sub(1)
            .context("no pending nested power mutation to finish")?;
        self.write(MUTATION, &marker)
            .with_context(|| self.mutation_recovery(&self.host, id))
    }

    fn lock_mutation(&self) -> Result<fs::File> {
        // Descendants borrow the parent hook lock. Serialize their marker
        // updates with each other and with recovery under a separate lock.
        fs::create_dir_all(&self.dir)?;
        let mut options = fs::File::options();
        options.create(true).truncate(false).read(true).write(true);
        #[cfg(unix)]
        std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
        let file = options.open(self.dir.join(MUTATION_LOCK))?;
        platform::lock_exclusive(&file).context("locking the power mutation record")?;
        Ok(file)
    }

    fn matching_mutation(&self, id: &str) -> Result<PowerMutation> {
        self.validate_id(id)?;
        let marker = self.mutation()?.ok_or_else(|| {
            error::PanioError::new(
                error::Kind::Usage,
                format!(
                    "power mutation {id} was not found for '{}'. {}",
                    self.target,
                    self.mutation_status(&self.host)
                ),
            )
            .target(&self.target)
        })?;
        if marker.id != id {
            return Err(self.mutation_refusal(&self.host, &marker));
        }
        Ok(marker)
    }

    fn read_mutation(&self, host: &str) -> Result<Option<PowerMutation>> {
        (|| {
            let marker = self.read::<PowerMutation>(MUTATION)?;
            if let Some(marker) = &marker {
                anyhow::ensure!(
                    valid_id(&marker.id) && valid_operation(&marker.operation),
                    "invalid power mutation record for '{}': {MUTATION}",
                    self.target
                );
            }
            Ok(marker)
        })()
        .with_context(|| self.mutation_status(host))
    }

    fn require_no_mutation(&self, host: &str) -> Result<()> {
        if let Some(marker) = self.read_mutation(host)? {
            return Err(self.mutation_refusal(host, &marker));
        }
        Ok(())
    }

    fn mutation_scope(&self, host: &str) -> String {
        format!(
            "-t {} --power-host {}",
            ssh::shell_quote(&self.target),
            ssh::shell_quote(host)
        )
    }

    fn mutation_status(&self, host: &str) -> String {
        format!(
            "Inspect with `panio action guard-status {}`",
            self.mutation_scope(host)
        )
    }

    fn mutation_recovery(&self, host: &str, id: &str) -> String {
        format!(
            "{}. After verifying the operation and any nested power commands stopped, run `panio action recover {} --id {id} --confirm-stopped`",
            self.mutation_status(host),
            self.mutation_scope(host)
        )
    }

    fn mutation_refusal(&self, host: &str, marker: &PowerMutation) -> anyhow::Error {
        error::PanioError::new(
            error::Kind::Usage,
            format!(
                "power mutation '{}' ({}) for '{}' may still be in progress. {}",
                marker.operation,
                marker.id,
                self.target,
                self.mutation_recovery(host, &marker.id)
            ),
        )
        .target(&self.target)
        .into()
    }

    pub fn release(&self, id: &str, peer: &Participant) -> Result<()> {
        self.clear_reservation(id, peer, false)
    }

    /// Fence an uncertain reserve even when it has yet to reach this host.
    pub fn cancel_reservation(&self, id: &str, peer: &Participant) -> Result<()> {
        self.clear_reservation(id, peer, true)
    }

    fn clear_reservation(&self, id: &str, peer: &Participant, cancel: bool) -> Result<()> {
        let mut power = self.bound_power(id, peer)?;
        if power.generation == peer.generation {
            let replace = match power.holder.as_deref() {
                Some(holder) if holder == id => true,
                Some(holder) if !cancel => return Err(self.refusal(holder, "power is reserved")),
                Some(_) => false,
                None => cancel,
            };
            if replace {
                power.generation = new_generation()?;
                power.holder = None;
                return self
                    .write(RESERVATION, &power)
                    .with_context(|| self.recovery(id));
            }
        }

        // A retry might see the generation from a rename whose directory sync
        // failed. A different active holder also fences this id, and its release
        // will advance the generation. Confirm whichever fence we observed.
        self.sync_record(RESERVATION)
            .with_context(|| self.recovery(id))
    }

    fn matching_ticket(&self, id: &str) -> Result<Ticket> {
        self.validate_id(id)?;
        match self.ticket()? {
            Some(ticket) if ticket.id == id => Ok(ticket),
            Some(ticket) => Err(self.refusal(&ticket.id, format!("action guard {id} is stale"))),
            None => Err(self.refusal(id, "the action receipt is missing")),
        }
    }

    fn set_phase(&self, ticket: &mut Ticket, phase: Phase) -> Result<()> {
        ticket.phase = phase;
        self.write(RECEIPT, ticket)
            .with_context(|| self.recovery(&ticket.id))
    }

    fn power_state(&self) -> Result<Option<PowerState>> {
        let power = self.read::<PowerState>(RESERVATION)?;
        if let Some(power) = &power {
            anyhow::ensure!(
                valid_id(&power.identity)
                    && valid_id(&power.generation)
                    && power.holder.as_deref().is_none_or(valid_id),
                "invalid power reservation for '{}': {RESERVATION}",
                self.target
            );
        }
        Ok(power)
    }

    fn bound_power(&self, id: &str, peer: &Participant) -> Result<PowerState> {
        self.validate_id(id)?;
        if !valid_id(&peer.generation) {
            return Err(self.refusal(id, "invalid power reservation generation"));
        }
        self.power_state()?
            .filter(|power| power.identity == peer.identity)
            .ok_or_else(|| self.refusal(id, "the power host no longer matches the action receipt"))
    }

    fn current_power(&self, id: &str, peer: &Participant) -> Result<PowerState> {
        let power = self.bound_power(id, peer)?;
        if power.generation != peer.generation {
            return Err(self.refusal(id, "the power reservation generation has already advanced"));
        }
        Ok(power)
    }

    fn validate_id(&self, id: &str) -> Result<()> {
        if !valid_id(id) {
            return Err(
                error::PanioError::new(error::Kind::Usage, "invalid action guard id")
                    .target(&self.target)
                    .into(),
            );
        }
        Ok(())
    }

    fn recovery(&self, id: &str) -> String {
        format!(
            "action guard {id} for '{}'. Recover with `panio action recover -t {} --id {id}`",
            self.target,
            ssh::shell_quote(&self.target)
        )
    }

    fn refusal(&self, id: &str, reason: impl std::fmt::Display) -> anyhow::Error {
        error::PanioError::new(
            error::Kind::Usage,
            format!("{reason}. {}", self.recovery(id)),
        )
        .target(&self.target)
        .into()
    }

    fn read<T: DeserializeOwned>(&self, name: &str) -> Result<Option<T>> {
        let path = self.dir.join(name);
        let context = || format!("reading {name} for '{}'", self.target);
        match fs::symlink_metadata(&path) {
            Ok(meta) => anyhow::ensure!(meta.is_file(), "{}: expected a regular file", context()),
            Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(err) => return Err(err).with_context(context),
        }
        let contents = match fs::read(&path) {
            Ok(contents) => contents,
            // An unlocked status inspection can race a completed removal.
            Err(err)
                if err.kind() == io::ErrorKind::NotFound
                    && matches!(fs::symlink_metadata(&path), Err(missing) if missing.kind() == io::ErrorKind::NotFound) =>
            {
                return Ok(None);
            }
            Err(err) => return Err(err).with_context(context),
        };
        anyhow::ensure!(
            contents.iter().find(|byte| !byte.is_ascii_whitespace()) == Some(&b'{'),
            "{}: expected a JSON object",
            context()
        );
        serde_json::from_slice(&contents)
            .map(Some)
            .with_context(context)
    }

    fn write(&self, name: &str, data: &impl Serialize) -> Result<()> {
        (|| -> Result<()> {
            fs::create_dir_all(&self.dir)?;
            // NamedTempFile starts owner-only on Unix. Persist its mode and
            // data before publishing it, without inheriting an older mode.
            let mut tmp = tempfile::NamedTempFile::new_in(&self.dir)?;
            serde_json::to_writer(&mut tmp, data)?;
            tmp.write_all(b"\n")?;
            platform::publish_state_file(tmp, &self.dir.join(name))?;
            self.sync_dirs()?;
            Ok(())
        })()
        .with_context(|| format!("writing {name} for '{}'", self.target))
    }

    fn remove(&self, name: &str) -> io::Result<()> {
        match fs::remove_file(self.dir.join(name)) {
            Ok(()) => self.sync_dirs(),
            // The deletion may be a retry after a failed directory sync.
            Err(err) if err.kind() == io::ErrorKind::NotFound && self.dir.try_exists()? => {
                self.sync_dirs()
            }
            Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(()),
            Err(err) => Err(err),
        }
    }

    #[cfg(unix)]
    fn sync_record(&self, _name: &str) -> io::Result<()> {
        self.sync_dirs()
    }

    #[cfg(windows)]
    fn sync_record(&self, name: &str) -> io::Result<()> {
        // Reestablish the published-file flush on retries if it failed after
        // the write-through rename made the record visible.
        fs::File::options()
            .write(true)
            .open(self.dir.join(name))?
            .sync_all()
    }

    fn sync_dirs(&self) -> io::Result<()> {
        // The target directory and its parents may have just been created.
        // Sync each relevant directory entry through the home directory.
        #[cfg(unix)]
        for dir in self.dir.ancestors() {
            let dir = if dir.as_os_str().is_empty() {
                std::path::Path::new(".")
            } else {
                dir
            };
            fs::File::open(dir)?.sync_all()?;
            if dir == self.home {
                break;
            }
        }
        Ok(())
    }
}

fn new_generation() -> Result<String> {
    platform::random_token().context("minting a power reservation generation")
}

fn valid_id(id: &str) -> bool {
    id.len() == 32
        && id
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

fn valid_operation(operation: &str) -> bool {
    !operation.trim().is_empty() && !operation.chars().any(char::is_control)
}

#[cfg(test)]
mod tests {
    use super::*;

    const FIRST: &str = "00000000000000000000000000000001";
    const SECOND: &str = "00000000000000000000000000000002";

    fn test_store() -> (tempfile::TempDir, Store) {
        let base = tempfile::tempdir().unwrap();
        let store = Store::at("target-machine", base.path().join("target-machine"));
        (base, store)
    }

    fn participants() -> [Participant; 2] {
        [
            Participant {
                host: "bench1".into(),
                identity: FIRST.into(),
                generation: SECOND.into(),
            },
            Participant {
                host: "bench2".into(),
                identity: SECOND.into(),
                generation: FIRST.into(),
            },
        ]
    }

    #[test]
    fn receipt_seal_and_forget_fence_delayed_execution() {
        let (_base, store) = test_store();
        let ticket = store.begin("boot-usb", "off", &participants()).unwrap();
        assert_eq!(ticket.phase, Phase::Ready);
        assert_eq!(ticket.hosts, participants());
        let reopened = Store::at("target-machine", store.dir.clone());
        let err = reopened
            .begin("boot-usb", "off", &participants())
            .unwrap_err();
        assert!(err.to_string().contains(&format!(
            "panio action recover -t target-machine --id {}",
            ticket.id
        )));
        assert!(reopened.start(FIRST).is_err());
        assert!(reopened.finish(&ticket.id).is_err());
        assert!(reopened.forget(&ticket.id).is_err());

        let sealed = reopened.seal(&ticket.id, false).unwrap();
        assert_eq!(sealed.phase, Phase::Finished);
        assert_eq!(reopened.seal(&ticket.id, false).unwrap(), sealed);
        assert!(reopened.start(&ticket.id).is_err());
        reopened.forget(&ticket.id).unwrap();
        assert!(store.ticket().unwrap().is_none());
        assert!(store.start(&ticket.id).is_err());
        let next = store.begin("boot-usb", "off", &participants()).unwrap();
        assert_ne!(next.id, ticket.id);
        assert!(reopened.start(&ticket.id).is_err());
        assert_eq!(store.ticket().unwrap(), Some(next));
    }

    #[test]
    fn reopened_running_receipt_requires_confirmation_before_recovery() {
        let (_base, store) = test_store();
        let ticket = store.begin("boot-usb", "off", &participants()).unwrap();
        store.start(&ticket.id).unwrap();

        let reopened = Store::at("target-machine", store.dir.clone());
        let err = reopened.seal(&ticket.id, false).unwrap_err().to_string();
        assert!(err.contains("--confirm-stopped"));
        assert_eq!(reopened.ticket().unwrap().unwrap().phase, Phase::Running);
        assert!(reopened.start(&ticket.id).is_err());
        assert!(reopened.forget(&ticket.id).is_err());
        let confirmed = reopened.seal(&ticket.id, true).unwrap();
        assert_eq!(confirmed.phase, Phase::Finished);
        assert!(reopened.finish(&ticket.id).is_err());
        reopened.forget(&ticket.id).unwrap();

        let next = reopened.begin("reset", "on", &participants()).unwrap();
        reopened.start(&next.id).unwrap();
        reopened.finish(&next.id).unwrap();
        assert_eq!(
            reopened.seal(&next.id, false).unwrap().phase,
            Phase::Finished
        );
        reopened.forget(&next.id).unwrap();
    }

    #[test]
    fn corrupt_and_incomplete_receipts_fail_closed() {
        let (_base, store) = test_store();
        let ticket = store.begin("boot-usb", "off", &participants()).unwrap();
        let path = store.dir.join(RECEIPT);
        let original = serde_json::to_value(&ticket).unwrap();
        let invalid_values = [
            ("required", serde_json::json!("unknown")),
            ("id", serde_json::json!("invalid")),
            ("action", serde_json::json!("")),
            ("phase", serde_json::json!("releasing")),
            ("hosts", serde_json::json!([])),
            ("hosts", serde_json::json!([{"host": "bench1"}])),
            (
                "hosts",
                serde_json::json!([participants()[0], participants()[0]]),
            ),
            ("unknown_field", serde_json::json!(true)),
        ];
        for (field, value) in invalid_values {
            let mut invalid = original.clone();
            invalid[field] = value;
            let bytes = serde_json::to_vec(&invalid).unwrap();
            fs::write(&path, &bytes).unwrap();
            assert!(store.ticket().is_err(), "accepted invalid field {field}");
            assert!(store.begin("boot-usb", "off", &participants()).is_err());
            assert!(store.start(&ticket.id).is_err());
            assert_eq!(fs::read(&path).unwrap(), bytes);
        }
        fs::write(&path, b"{\"id\":").unwrap();
        assert!(store.ticket().is_err());
        let array = serde_json::json!([
            ticket.id,
            ticket.action,
            ticket.required,
            ticket.hosts,
            ticket.phase
        ]);
        fs::write(&path, serde_json::to_vec(&array).unwrap()).unwrap();
        assert!(store.ticket().is_err());
    }

    #[test]
    fn power_state_refuses_wrong_id_and_wrong_host_after_restart() {
        let (_base, store) = test_store();
        store.power_available().unwrap();
        assert!(!store.dir.exists());
        let peer = store.participant("bench1").unwrap();
        store.release(FIRST, &peer).unwrap();
        store.reserve(FIRST, &peer).unwrap();

        let reopened = Store::at("target-machine", store.dir.clone());
        assert_eq!(reopened.participant("bench1").unwrap(), peer);
        assert!(reopened.power_available().is_err());
        reopened.assert_reserved(FIRST, &peer).unwrap();
        reopened.reserve(FIRST, &peer).unwrap();
        assert!(reopened.assert_reserved(SECOND, &peer).is_err());
        assert!(reopened.reserve(SECOND, &peer).is_err());
        assert!(reopened.release(SECOND, &peer).is_err());

        let (_replacement_base, replacement) = test_store();
        assert!(replacement.release(FIRST, &peer).is_err());
        assert!(replacement.cancel_reservation(FIRST, &peer).is_err());
        assert!(!replacement.dir.exists());
        let other = replacement.participant("bench1").unwrap();
        assert_ne!(other.identity, peer.identity);
        let wrong_host = Participant {
            generation: peer.generation.clone(),
            ..other
        };
        assert!(reopened.release(FIRST, &wrong_host).is_err());
        assert!(reopened.cancel_reservation(FIRST, &wrong_host).is_err());
        assert!(reopened.assert_reserved(FIRST, &wrong_host).is_err());
        assert!(replacement.release(FIRST, &peer).is_err());
        assert!(replacement.reserve(FIRST, &peer).is_err());
        reopened.assert_reserved(FIRST, &peer).unwrap();
    }

    #[cfg(windows)]
    #[test]
    fn windows_release_replaces_held_record_and_clears_temporary_attribute() {
        use std::os::windows::fs::MetadataExt;
        use windows_sys::Win32::Storage::FileSystem::FILE_ATTRIBUTE_TEMPORARY;

        let base = tempfile::tempdir().unwrap();
        let store = Store::at("target-machine", base.path().join("target-é"));
        let peer = store.participant("bench1").unwrap();
        store.reserve(FIRST, &peer).unwrap();
        assert_eq!(
            store.power_state().unwrap().unwrap().holder.as_deref(),
            Some(FIRST)
        );
        store.release(FIRST, &peer).unwrap();

        let reopened = Store::at("target-machine", store.dir.clone());
        let released = reopened.power_state().unwrap().unwrap();
        assert_eq!(released.identity, peer.identity);
        assert_ne!(released.generation, peer.generation);
        assert_eq!(released.holder, None);
        assert!(reopened.reserve(FIRST, &peer).is_err());
        let attributes = fs::metadata(reopened.dir.join(RESERVATION))
            .unwrap()
            .file_attributes();
        assert_eq!(attributes & FILE_ATTRIBUTE_TEMPORARY, 0);
    }

    #[test]
    fn normal_release_fences_late_reserve_and_preserves_newer_owner() {
        let (_base, store) = test_store();
        let first_peer = store.participant("bench1").unwrap();
        store.reserve(FIRST, &first_peer).unwrap();
        store.release(FIRST, &first_peer).unwrap();

        let reopened = Store::at("target-machine", store.dir.clone());
        reopened.power_available().unwrap();
        assert!(reopened.reserve(FIRST, &first_peer).is_err());
        assert!(reopened.assert_reserved(FIRST, &first_peer).is_err());
        let second_peer = reopened.participant("bench1").unwrap();
        assert_eq!(first_peer.identity, second_peer.identity);
        assert_ne!(first_peer.generation, second_peer.generation);
        reopened.reserve(SECOND, &second_peer).unwrap();
        reopened.release(FIRST, &first_peer).unwrap();
        reopened.cancel_reservation(FIRST, &first_peer).unwrap();
        assert!(reopened.reserve(FIRST, &first_peer).is_err());
        reopened.assert_reserved(SECOND, &second_peer).unwrap();
    }

    #[test]
    fn recovery_fences_unreceived_reserve_and_respects_same_generation_owner() {
        let (_base, store) = test_store();
        let first_peer = store.participant("bench1").unwrap();
        store.cancel_reservation(FIRST, &first_peer).unwrap();
        store.power_available().unwrap();
        assert!(store.reserve(FIRST, &first_peer).is_err());

        let second_peer = store.participant("bench1").unwrap();
        store.reserve(SECOND, &second_peer).unwrap();
        store.cancel_reservation(FIRST, &second_peer).unwrap();
        assert!(store.reserve(FIRST, &second_peer).is_err());
        store.assert_reserved(SECOND, &second_peer).unwrap();
        store.cancel_reservation(SECOND, &second_peer).unwrap();
        store.power_available().unwrap();
        assert!(store.reserve(FIRST, &second_peer).is_err());
        assert!(store.reserve(SECOND, &second_peer).is_err());
        assert!(store.cancel_reservation("../unsafe", &second_peer).is_err());
    }

    #[test]
    fn normal_completion_and_recovery_use_bounded_disk_state() {
        let (_base, store) = test_store();
        let mut expired = Vec::new();
        for run in 0..12 {
            let peer = store.participant("bench1").unwrap();
            let ticket = store
                .begin("boot-usb", "off", std::slice::from_ref(&peer))
                .unwrap();
            if run % 2 == 0 {
                store.reserve(&ticket.id, &peer).unwrap();
                store.start(&ticket.id).unwrap();
                store.finish(&ticket.id).unwrap();
                store.release(&ticket.id, &peer).unwrap();
            } else {
                store.seal(&ticket.id, false).unwrap();
                store.cancel_reservation(&ticket.id, &peer).unwrap();
            }
            store.forget(&ticket.id).unwrap();
            expired.push((ticket.id, peer));
            let mutation = store.begin_mutation("power off").unwrap();
            store.finish_mutation(&mutation).unwrap();
        }
        for (id, peer) in expired {
            assert!(store.reserve(&id, &peer).is_err());
        }
        store.power_available().unwrap();
        let mut files: Vec<_> = fs::read_dir(&store.dir)
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .collect();
        files.sort();
        assert_eq!(files, [MUTATION_LOCK, RESERVATION]);
    }

    #[test]
    fn corrupt_or_incomplete_power_state_fails_closed() {
        let (_base, store) = test_store();
        let peer = store.participant("bench1").unwrap();
        store.reserve(FIRST, &peer).unwrap();
        let path = store.dir.join(RESERVATION);
        let original: serde_json::Value =
            serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        let mut missing_holder = original.clone();
        missing_holder.as_object_mut().unwrap().remove("holder");
        let mut invalid_holder = original.clone();
        invalid_holder["holder"] = serde_json::json!("invalid");
        let mut invalid_generation = original.clone();
        invalid_generation["generation"] = serde_json::json!("invalid");
        let mut invalid_identity = original;
        invalid_identity["identity"] = serde_json::json!("invalid");

        for corrupt in [
            missing_holder,
            invalid_holder,
            invalid_generation,
            invalid_identity,
        ] {
            let contents = serde_json::to_vec(&corrupt).unwrap();
            fs::write(&path, &contents).unwrap();
            assert!(store.power_available().is_err());
            assert!(store.participant("bench1").is_err());
            assert!(store.reserve(FIRST, &peer).is_err());
            assert!(store.assert_reserved(FIRST, &peer).is_err());
            assert!(store.release(FIRST, &peer).is_err());
            assert!(store.cancel_reservation(FIRST, &peer).is_err());
            assert_eq!(fs::read(&path).unwrap(), contents);
        }
    }

    #[test]
    fn power_mutation_excludes_delayed_reserve_and_new_power_mutations() {
        let (_base, store) = test_store();
        let peer = store.participant("bench1").unwrap();
        let id = store.begin_mutation("power-cycle").unwrap();
        let reopened = Store::at("target-machine", store.dir.clone());
        assert_eq!(
            reopened.mutation().unwrap(),
            Some(PowerMutation {
                id: id.clone(),
                operation: "power-cycle".into(),
                pending_children: 0,
            }),
        );
        assert!(reopened.power_available().is_err());
        assert!(reopened.begin_mutation("power off").is_err());
        assert!(reopened.participant("bench1").is_err());
        assert!(reopened.assert_reserved(FIRST, &peer).is_err());
        let message = reopened.reserve(FIRST, &peer).unwrap_err().to_string();
        assert!(message.contains("panio action guard-status -t target-machine --power-host bench1"));
        assert!(message.contains(&format!(
            "panio action recover -t target-machine --power-host bench1 --id {id} --confirm-stopped"
        )));

        reopened.finish_mutation(&id).unwrap();
        reopened.finish_mutation(&id).unwrap();
        reopened.reserve(FIRST, &peer).unwrap();
        assert!(reopened.begin_mutation("power off").is_err());
        assert!(reopened.mutation().unwrap().is_none());
        reopened.release(FIRST, &peer).unwrap();
        reopened.begin_mutation("power off").unwrap();
    }

    #[test]
    fn concurrent_reserve_and_mutation_start_allow_only_one_to_publish() {
        let (_base, store) = test_store();
        for _ in 0..8 {
            let peer = store.participant("bench1").unwrap();
            let start = std::sync::Barrier::new(2);
            let (mutation, reservation) = std::thread::scope(|scope| {
                let mutation = scope.spawn(|| {
                    start.wait();
                    store.begin_mutation("power-cycle")
                });
                let reservation = scope.spawn(|| {
                    start.wait();
                    store.reserve(FIRST, &peer)
                });
                (mutation.join().unwrap(), reservation.join().unwrap())
            });
            match (mutation, reservation) {
                (Ok(id), Err(_)) => {
                    assert_eq!(store.mutation().unwrap().unwrap().id, id);
                    assert!(store.power_state().unwrap().unwrap().holder.is_none());
                    store.finish_mutation(&id).unwrap();
                }
                (Err(_), Ok(())) => {
                    assert!(store.mutation().unwrap().is_none());
                    store.assert_reserved(FIRST, &peer).unwrap();
                    store.release(FIRST, &peer).unwrap();
                }
                results => panic!("expected exactly one operation to succeed, got {results:?}"),
            }
        }
    }

    #[test]
    fn mutation_recovery_needs_exact_id_and_confirmation() {
        let (_base, store) = test_store();
        let id = store.begin_mutation("serial button").unwrap();
        store.begin_nested_mutation(&id).unwrap();
        assert!(store.resume_mutation(FIRST, true).is_err());
        assert!(store.resume_mutation(&id, false).is_err());
        assert!(store.finish_mutation(&id).is_err());
        assert_eq!(store.mutation().unwrap().unwrap().id, id);

        store.resume_mutation(&id, true).unwrap();
        assert!(store.begin_nested_mutation(&id).is_err());
        assert!(store.finish_nested_mutation(&id).is_err());
        assert!(store.mutation().unwrap().is_none());

        let replacement = store.begin_mutation("power on").unwrap();
        assert!(store.begin_nested_mutation(&id).is_err());
        assert!(store.finish_nested_mutation(&id).is_err());
        assert!(store.finish_mutation(&id).is_err());
        let current = store.mutation().unwrap().unwrap();
        assert_eq!(current.id, replacement);
        assert_eq!(current.pending_children, 0);
    }

    #[test]
    fn concurrent_nested_updates_are_retained_until_each_child_finishes() {
        let (_base, store) = test_store();
        let id = store.begin_mutation("power on").unwrap();
        let mutator = &store;
        let id = id.as_str();
        std::thread::scope(|scope| {
            for _ in 0..8 {
                scope.spawn(move || mutator.begin_nested_mutation(id).unwrap());
            }
        });
        assert_eq!(store.mutation().unwrap().unwrap().pending_children, 8);
        assert!(store.finish_mutation(id).is_err());
        std::thread::scope(|scope| {
            for _ in 0..7 {
                scope.spawn(move || mutator.finish_nested_mutation(id).unwrap());
            }
        });
        assert_eq!(store.mutation().unwrap().unwrap().pending_children, 1);
        assert!(store.finish_mutation(id).is_err());
        store.finish_nested_mutation(id).unwrap();
        store.finish_mutation(id).unwrap();
        assert!(store.mutation().unwrap().is_none());
    }

    #[test]
    fn corrupt_mutation_record_fails_closed() {
        let (_base, store) = test_store();
        let peer = store.participant("bench1").unwrap();
        let id = store.begin_mutation("power off").unwrap();
        let path = store.dir.join(MUTATION);
        let original: serde_json::Value =
            serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        let mut missing_count = original.clone();
        missing_count
            .as_object_mut()
            .unwrap()
            .remove("pending_children");
        let mut invalid_operation = original.clone();
        invalid_operation["operation"] = serde_json::json!(" ");
        let mut invalid_id = original;
        invalid_id["id"] = serde_json::json!("bad id");
        for invalid in [missing_count, invalid_id, invalid_operation] {
            let bytes = serde_json::to_vec(&invalid).unwrap();
            fs::write(&path, &bytes).unwrap();
            assert!(store.mutation().is_err());
            assert!(store.power_available().is_err());
            assert!(store.begin_mutation("power off").is_err());
            assert!(store.participant("bench1").is_err());
            assert!(store.reserve(FIRST, &peer).is_err());
            assert!(store.assert_reserved(FIRST, &peer).is_err());
            assert!(store.finish_mutation(&id).is_err());
            assert!(store.resume_mutation(&id, true).is_err());
            assert_eq!(fs::read(&path).unwrap(), bytes);
        }
    }

    #[cfg(unix)]
    #[test]
    fn state_is_owner_only_and_power_marker_cannot_be_a_symlink() {
        use std::os::unix::fs::{symlink, PermissionsExt};

        let (_base, store) = test_store();
        let peer = store.participant("bench1").unwrap();
        let ticket = store.begin("boot-usb", "off", &participants()).unwrap();
        store.reserve(&ticket.id, &peer).unwrap();
        for name in [RECEIPT, RESERVATION] {
            let mode = fs::metadata(store.dir.join(name))
                .unwrap()
                .permissions()
                .mode();
            assert_eq!(mode & 0o777, 0o600);
        }
        store.release(&ticket.id, &peer).unwrap();
        fs::remove_file(store.dir.join(RESERVATION)).unwrap();
        symlink("missing-marker", store.dir.join(RESERVATION)).unwrap();
        assert!(store.power_available().is_err());
        assert!(store.participant("bench1").is_err());
        assert!(store.reserve(&ticket.id, &peer).is_err());
    }
}
