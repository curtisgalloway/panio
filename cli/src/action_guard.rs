// SPDX-FileCopyrightText: 2026 The Panio Authors
// SPDX-License-Identifier: Apache-2.0

//! A split-host action cannot hold a power host's file lock over SSH: losing
//! the connection could free it before the action exits. Instead the action
//! host issues a unique receipt. Every power control records that ID before
//! we read power. Recovery fences execution on the action host first.

use anyhow::{bail, ensure, Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::action_guard_store::{Participant, Phase, Store, Ticket};
use crate::model::{ChannelKind, Lab};
use crate::{actions, dispatch, error, model};

/// Returned by the power host only after reading the required state while
/// the matching reservation is held. The action host checks its operation
/// and participant against the current receipt before launching the hook.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PowerCheck {
    id: String,
    peer: Participant,
    required: String,
    action: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Step {
    Identify {
        host: String,
    },
    Inspect,
    InspectMutation,
    ResumeMutation {
        id: String,
        confirm_stopped: bool,
    },
    Begin {
        action: String,
        required: String,
        hosts: Vec<Participant>,
    },
    Reserve {
        id: String,
        peer: Participant,
    },
    Check(PowerCheck),
    Execute {
        checked: PowerCheck,
    },
    Seal {
        id: String,
        confirm_stopped: bool,
        expect_finished: bool,
    },
    Cancel {
        id: String,
        peer: Participant,
    },
    Forget {
        id: String,
    },
}

impl std::str::FromStr for Step {
    type Err = serde_json::Error;

    fn from_str(s: &str) -> std::result::Result<Self, Self::Err> {
        serde_json::from_str(s)
    }
}

fn local_step(lab: &Lab, target: &str, step: &Step) -> Result<Option<Value>> {
    if !lab.targets.contains_key(target) {
        return Err(error::target_not_found(target).into());
    }
    let store = Store::for_target(target);
    // Reading an atomic record is safe while a long-running hook holds the lock.
    match step {
        Step::Inspect => return Ok(Some(serde_json::to_value(store.ticket()?)?)),
        Step::InspectMutation => return Ok(Some(serde_json::to_value(store.mutation()?)?)),
        _ => {}
    }
    let lock = actions::guard_protocol_lock(target)?;
    match step {
        Step::Inspect | Step::InspectMutation => unreachable!(),
        Step::ResumeMutation {
            id,
            confirm_stopped,
        } => store.resume_mutation(id, *confirm_stopped)?,
        Step::Identify { host } => {
            return Ok(Some(serde_json::to_value(store.participant(host)?)?))
        }
        Step::Begin {
            action,
            required,
            hosts,
        } => {
            let a = actions::find(lab, target, action)?;
            if a.requires_power.as_ref() != Some(required) {
                bail!("action power requirement changed");
            }
            return Ok(Some(serde_json::to_value(
                store.begin(action, required, hosts)?,
            )?));
        }
        Step::Reserve { id, peer } => store.reserve(id, peer)?,
        Step::Cancel { id, peer } => store.cancel_reservation(id, peer)?,
        Step::Check(check) => {
            store.assert_reserved(&check.id, &check.peer)?;
            let is_on = crate::local_power_is_on_locked(lab, target, &lock)?;
            if let Some(refused) =
                actions::power_refusal(target, &check.action, &check.required, is_on)
            {
                return Err(refused.into());
            }
            return Ok(Some(serde_json::to_value(check)?));
        }
        Step::Execute { checked } => {
            let id = &checked.id;
            let current = store.ticket()?.ok_or_else(|| {
                error::PanioError::new(error::Kind::Usage, "action receipt is missing")
                    .target(target)
            })?;
            ensure!(
                current.id == *id
                    && current.required == checked.required
                    && current.action == checked.action
                    && current.hosts.contains(&checked.peer),
                "power check does not match the action receipt"
            );
            let a = actions::find(lab, target, &current.action)?;
            ensure!(
                a.requires_power.as_ref() == Some(&current.required),
                "action power requirement changed"
            );
            actions::run_reserved_hook(&store, id, &lock, target, &current.action, &a.cmd)?;
        }
        Step::Seal {
            id,
            confirm_stopped,
            expect_finished,
        } => {
            if *expect_finished {
                ensure!(
                    store
                        .ticket()?
                        .is_some_and(|t| t.id == *id && t.phase == Phase::Finished),
                    "action host did not record completion"
                );
            }

            return Ok(Some(serde_json::to_value(
                store.seal(id, *confirm_stopped)?,
            )?));
        }
        Step::Forget { id } => store.forget(id)?,
    }
    Ok(None)
}

pub fn serve(lab: &Lab, target: &str, request: &Step) -> Result<()> {
    if let Some(reply) = local_step(lab, target, request)? {
        println!("{reply}");
    }
    Ok(())
}

struct Coordinator<'a> {
    lab: &'a Lab,
    target: &'a str,
    action_host: &'a str,
}

impl Coordinator<'_> {
    fn call(&self, host: &str, step: Step) -> Result<Value> {
        if host != model::LOCAL && !self.lab.hosts.contains_key(host) {
            bail!("recorded guard host '{host}' is missing from the lab; restore its binding to recover");
        }
        if self.lab.host(host).is_local(host) {
            return Ok(local_step(self.lab, self.target, &step)?.unwrap_or(Value::Null));
        }
        let request = serde_json::to_string(&step)?;
        let args = ["action", "guard-step", "-t", self.target, &request];
        let out = dispatch::run_subcommand(self.lab, self.target, host, &args)?;
        if out.status != 0 {
            return Err(crate::remote_failure(
                out.status,
                self.target,
                out.stderr.trim().to_owned(),
            ));
        }
        if matches!(step, Step::Check(_)) {
            // Preserve the state hook's diagnostics. Successful cleanup
            // steps must leave the executed command's JSON error last.
            eprint!("{}", out.stderr);
        }
        match step {
            Step::Check(_)
            | Step::Identify { .. }
            | Step::Begin { .. }
            | Step::Seal { .. }
            | Step::Inspect
            | Step::InspectMutation => {
                serde_json::from_str(out.stdout.trim()).context("invalid action guard reply")
            }
            _ => Ok(Value::Null),
        }
    }

    fn execute(&self, checked: PowerCheck) -> Result<i32> {
        let host = self.action_host;
        let step = Step::Execute { checked };
        if self.lab.host(host).is_local(host) {
            local_step(self.lab, self.target, &step)?;
            return Ok(0);
        }
        // The actual action preserves stdin, streaming output and the remote
        // CLI's structured error. The other protocol steps exchange replies.
        let argv = [
            "action".to_owned(),
            "guard-step".to_owned(),
            "-t".to_owned(),
            self.target.to_owned(),
            serde_json::to_string(&step)?,
        ];
        dispatch::dispatch(self.lab, self.target, host, dispatch::Mode::Reexec, &argv)
    }

    fn participant(&self, host: &str) -> Result<Participant> {
        let participant: Participant = serde_json::from_value(self.call(
            host,
            Step::Identify {
                host: host.to_owned(),
            },
        )?)?;
        ensure!(
            participant.host == host,
            "power host returned a different guard participant"
        );
        Ok(participant)
    }

    fn release(&self, ticket: &Ticket) -> Result<()> {
        for peer in &ticket.hosts {
            self.call(
                &peer.host,
                Step::Cancel {
                    id: ticket.id.clone(),
                    peer: peer.clone(),
                },
            )?;
        }
        self.call(
            self.action_host,
            Step::Forget {
                id: ticket.id.clone(),
            },
        )?;
        Ok(())
    }

    fn close(&self, id: &str, confirm_stopped: bool, expect_finished: bool) -> Result<()> {
        // Seal first. An execute request delayed in transit can no longer
        // start after recovery returns power to a caller.
        let ticket: Ticket = serde_json::from_value(self.call(
            self.action_host,
            Step::Seal {
                id: id.to_owned(),
                confirm_stopped,
                expect_finished,
            },
        )?)?;
        ensure!(
            ticket.id == id && ticket.phase == Phase::Finished,
            "action host did not seal the requested receipt"
        );
        self.release(&ticket)
    }

    fn run(&self, name: &str, required: &str, power_host: &str) -> Result<()> {
        let target = &self.lab.targets[self.target];
        let power = self.participant(power_host)?;
        let mut hosts = vec![power.clone()];
        for port in target.serial.iter().filter(|p| p.power_button) {
            let host = port.host.as_deref().unwrap_or(target.default_host());
            if !hosts.iter().any(|p| p.host == host) {
                let peer = self.participant(host)?;
                if !hosts.iter().any(|p| p.identity == peer.identity) {
                    hosts.push(peer);
                }
            }
        }
        // Stable order also applies when two lab files use different host aliases.
        hosts.sort_by(|first, second| first.identity.cmp(&second.identity));
        let begin = Step::Begin {
            action: name.to_owned(),
            required: required.to_owned(),
            hosts: hosts.clone(),
        };
        let reply = self.call(self.action_host, begin).with_context(|| {
            format!("starting guarded action; use 'panio action guard-status -t {}' to inspect an interrupted start", self.target)
        })?;
        let ticket: Ticket = serde_json::from_value(reply).context("invalid action receipt")?;
        ensure!(
            ticket.phase == Phase::Ready && ticket.action == name && ticket.required == required && ticket.hosts == hosts,
            "action host returned a different guard receipt; inspect 'panio action guard-status -t {}' before retrying", self.target
        );
        let id = &ticket.id;
        let result = (|| -> Result<i32> {
            for peer in &ticket.hosts {
                self.call(
                    &peer.host,
                    Step::Reserve {
                        id: id.clone(),
                        peer: peer.clone(),
                    },
                )?;
            }
            let expected = PowerCheck {
                id: id.clone(),
                peer: power,
                required: required.to_owned(),
                action: name.to_owned(),
            };
            let checked: PowerCheck =
                serde_json::from_value(self.call(power_host, Step::Check(expected.clone()))?)
                    .context("invalid guarded power-state reply")?;
            ensure!(
                checked == expected,
                "power-state reply does not match the guarded operation"
            );
            self.execute(checked)
        })();
        let cleanup = self.close(id, false, matches!(result, Ok(0)));
        if let Err(e) = cleanup {
            let original = match result {
                Ok(0) => String::new(),
                Ok(code) => format!("action host exited with code {code}; "),
                Err(e) => format!("{e:#}; "),
            };
            return Err(e.context(format!(
                "{original}guard recovery incomplete; run 'panio action recover -t {} --id {id}'",
                self.target
            )));
        }
        let code = result?;
        if code != 0 {
            // The remote has already reported the error. Exit after sealing
            // its receipt and releasing the reservations.
            std::process::exit(code);
        }
        println!("Action '{name}' complete.");
        Ok(())
    }
}

pub fn run_split(
    lab: &Lab,
    target: &str,
    name: &str,
    required: &str,
    power_host: &str,
    action_host: &str,
) -> Result<()> {
    Coordinator {
        lab,
        target,
        action_host,
    }
    .run(name, required, power_host)
}

fn action_host(lab: &Lab, target: &str) -> Result<String> {
    let rt = lab
        .resolved_target(target)
        .ok_or_else(|| error::target_not_found(target))?;
    Ok(model::channel_host(&rt, ChannelKind::Actions, None)?)
}

pub fn status(lab: &Lab, target: &str, power_host: Option<&str>) -> Result<()> {
    let host = match power_host {
        Some(host) => host.to_owned(),
        None => action_host(lab, target)?,
    };
    let row = Coordinator {
        lab,
        target,
        action_host: &host,
    }
    .call(
        &host,
        if power_host.is_some() {
            Step::InspectMutation
        } else {
            Step::Inspect
        },
    )?;
    println!("{}", serde_json::to_string_pretty(&row)?);
    Ok(())
}

pub fn recover(
    lab: &Lab,
    target: &str,
    id: &str,
    confirm_stopped: bool,
    power_host: Option<&str>,
) -> Result<()> {
    if let Some(host) = power_host {
        Coordinator {
            lab,
            target,
            action_host: host,
        }
        .call(
            host,
            Step::ResumeMutation {
                id: id.to_owned(),
                confirm_stopped,
            },
        )?;
        println!("Cleared the interrupted power command for '{target}' on '{host}'.");
        return Ok(());
    }
    let host = action_host(lab, target)?;
    Coordinator {
        lab,
        target,
        action_host: &host,
    }
    .close(id, confirm_stopped, false)?;
    println!("Released power reservations for '{target}'.");
    Ok(())
}
