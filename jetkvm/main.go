// Copyright 2026 Curtis Galloway
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

// Command jetkvm drives a JetKVM network KVM's keyboard and mouse over its
// WebRTC data channel, speaking panio's HID serial protocol.
//
// It is a sibling of hidrig and ch9329: the same command vocabulary (type,
// key, combo, down, up, releaseall, move, moveabs, click, mdown, mup, scroll,
// ping, version, run) and the same serve/stop daemon, so it drops into a
// panio `hid` channel:
//
//	panio hid set -t target-machine --cmd "jetkvm -d 192.0.2.10"
//
// JetKVM firmware exposes keyboard and mouse only over WebRTC and allows ONE
// session at a time, so a one-shot opens a session, acts, and closes it; the
// serve daemon holds one session and every one-shot routes through it.
package main

import (
	"bytes"
	"context"
	"encoding/json"
	"errors"
	"flag"
	"fmt"
	"io"
	"log/slog"
	"net/http"
	"os"
	"strings"
	"time"
)

const (
	exitOK            = 0
	exitFailure       = 1
	exitUsage         = 2
	exitNotConfigured = 3
)

const usageText = `jetkvm -- JetKVM network KVM keyboard/mouse injection (panio hid helper)

usage: jetkvm -d <host[:port]> [options] <command> [args]

options:
  -d, --device <host[:port]>   the JetKVM's address (required for commands that act on it)
  --target <name>              panio target name (selects the daemon's runtime dir)
  --password-file <path>       read the device password from a file
  --password-command <cmd>     run a command (sh -c) that prints the password
  The password is also read from the JETKVM_PASSWORD environment variable,
  which takes precedence. It is never accepted as a flag value.

commands:
  type <text>            type text (US layout)
  key <NAME>             tap a key
  combo <NAME>...        chord (at most 6 keys)
  down <NAME> | up <NAME>  hold / release a key
  releaseall | release   release every held key and button
  move <dx> <dy>         relative mouse move
  moveabs <x> <y>        absolute mouse move, 0..32767
  click [button]         click left (default), right or middle
  mdown [button] | mup [button]
  scroll <amount>        wheel, positive = up
  ping                   RPC liveness check
  version                protocol version and capabilities
  info                   USB, video and keyboard-LED state
  run <file|-> [--delay-ms N]   run a command file
  serve [--port N] [--ffmpeg PATH]
                         run the KVM daemon (holds the one session; serves
                         video as RFB on GET /rfb when ffmpeg is available)
  stop                   stop the running daemon

JetKVM allows one session at a time: opening one (a one-shot, the daemon, or
a browser tab) closes whichever other session was open.
`

func main() {
	os.Exit(run(os.Args[1:], os.Stdin, os.Stdout, os.Stderr))
}

type globals struct {
	device  string
	target  string
	sources PasswordSources
}

func run(args []string, stdin io.Reader, stdout, stderr io.Writer) int {
	fs := flag.NewFlagSet("jetkvm", flag.ContinueOnError)
	fs.SetOutput(stderr)
	fs.Usage = func() { fmt.Fprint(stderr, usageText) }
	var g globals
	fs.StringVar(&g.device, "d", "", "")
	fs.StringVar(&g.device, "device", "", "")
	fs.StringVar(&g.target, "target", "", "")
	fs.StringVar(&g.sources.File, "password-file", "", "")
	fs.StringVar(&g.sources.Command, "password-command", "", "")
	if err := fs.Parse(args); err != nil {
		if errors.Is(err, flag.ErrHelp) {
			return exitOK
		}
		return exitUsage
	}
	rest := fs.Args()
	if len(rest) == 0 {
		fmt.Fprint(stderr, usageText)
		return exitUsage
	}
	cmd, cargs := rest[0], rest[1:]
	err := dispatchCommand(g, cmd, cargs, stdin, stdout, stderr)
	return report(err, stderr)
}

type usageError struct{ msg string }

func (e *usageError) Error() string { return e.msg }

func report(err error, stderr io.Writer) int {
	if err == nil {
		return exitOK
	}
	fmt.Fprintf(stderr, "jetkvm: %v\n", err)
	var nc *NotConfigured
	var ue *usageError
	switch {
	case errors.As(err, &nc), errors.Is(err, ErrAuth):
		return exitNotConfigured
	case errors.As(err, &ue):
		return exitUsage
	}
	return exitFailure
}

func dispatchCommand(g globals, cmd string, args []string, stdin io.Reader, stdout, stderr io.Writer) error {
	switch cmd {
	case "help", "-h", "--help":
		fmt.Fprint(stdout, usageText)
		return nil
	case "stop":
		// `stop --target T` must work as well as `--target T stop`: the global
		// FlagSet stops at the first non-flag word, so a --target written
		// after the verb was silently dropped and stop looked in the
		// untargeted runtime dir.
		sfs := flag.NewFlagSet("stop", flag.ContinueOnError)
		sfs.SetOutput(io.Discard)
		sfs.StringVar(&g.target, "target", g.target, "")
		if err := sfs.Parse(args); err != nil {
			return &usageError{fmt.Sprintf("stop: %v", err)}
		}
		if sfs.NArg() > 0 {
			return &usageError{fmt.Sprintf("stop: unexpected argument %q", sfs.Arg(0))}
		}
		return cmdStop(g, stdout)
	case "serve":
		return cmdServe(g, args)
	case "version":
		fmt.Fprintln(stdout, versionReply)
		return nil
	}
	line, err := buildLine(cmd, args)
	if err != nil {
		return err
	}
	if g.device == "" {
		return &usageError{"required argument '--device <host[:port]>' (-d) was not provided"}
	}
	if _, err := splitHost(g.device); err != nil {
		return &usageError{err.Error()}
	}
	switch cmd {
	case "run":
		return cmdRun(g, args, stdin, stdout)
	}
	tx, err := openSender(g)
	if err != nil {
		return err
	}
	defer tx.Close()
	data, err := tx.RunLine(line)
	if err != nil {
		return err
	}
	if cmd == "info" {
		if ds, ok := tx.(*daemonSender); ok {
			if vs := ds.videoStream(); vs != "" {
				data += " stream=" + vs
			}
		}
		fmt.Fprintln(stdout, data)
	} else {
		fmt.Fprintln(stdout, "OK")
	}
	return nil
}

// buildLine turns a subcommand and its arguments into a protocol line.
func buildLine(cmd string, args []string) (string, error) {
	need := func(n int) error {
		if len(args) < n {
			return &usageError{fmt.Sprintf("%s needs %d argument(s)", cmd, n)}
		}
		return nil
	}
	switch cmd {
	case "type":
		if err := need(1); err != nil {
			return "", err
		}
		return "type " + strings.Join(args, " "), nil
	case "key", "down", "up":
		if err := need(1); err != nil {
			return "", err
		}
		return cmd + " " + args[0], nil
	case "combo":
		if err := need(1); err != nil {
			return "", err
		}
		return "combo " + strings.Join(args, " "), nil
	case "releaseall", "release", "ping", "info":
		return cmd, nil
	case "move", "moveabs":
		if err := need(2); err != nil {
			return "", err
		}
		a, errA := parseInt(args[0])
		b, errB := parseInt(args[1])
		if errA != nil || errB != nil {
			return "", &usageError{cmd + " needs two integer arguments"}
		}
		if cmd == "moveabs" {
			a, b = clampAbs(a), clampAbs(b)
		}
		return fmt.Sprintf("%s %d %d", cmd, a, b), nil
	case "click", "mdown", "mup":
		if len(args) == 0 {
			return cmd + " left", nil
		}
		return cmd + " " + args[0], nil
	case "scroll":
		if err := need(1); err != nil {
			return "", err
		}
		return "scroll " + args[0], nil
	case "run":
		return "", nil
	}
	return "", &usageError{fmt.Sprintf("unknown command %q (try `jetkvm help`)", cmd)}
}

// Sender runs command lines through a running daemon or, failing that,
// through a session of its own.
type Sender interface {
	RunLine(line string) (string, error)
	Close()
}

func openSender(g globals) (Sender, error) {
	if d := discover(g.target); d != nil && d.Device == g.device {
		return &daemonSender{base: fmt.Sprintf("http://127.0.0.1:%d", d.Port), token: d.Token}, nil
	}
	pw, err := resolvePassword(g.sources)
	if err != nil {
		return nil, err
	}
	ctx, cancel := context.WithTimeout(context.Background(), dialTimeout)
	defer cancel()
	l, err := Dial(ctx, DialConfig{Host: g.device, Password: pw})
	if err != nil {
		return nil, err
	}
	return &directSender{link: l, session: NewSession(linkCaller{l})}, nil
}

type linkCaller struct{ l *Link }

func (c linkCaller) Call(method string, params any) (json.RawMessage, error) {
	ctx, cancel := context.WithTimeout(context.Background(), rpcTimeout)
	defer cancel()
	return c.l.Call(ctx, method, params)
}

type directSender struct {
	link    *Link
	session *Session
}

func (d *directSender) RunLine(line string) (string, error) { return executeLine(d.session, line) }
func (d *directSender) Close()                              { d.link.Close() }

type daemonSender struct {
	base  string
	token string
}

func (d *daemonSender) Close() {}

// videoStream returns the daemon's /status "video" object as compact JSON, or
// "" when the daemon cannot be asked.
func (d *daemonSender) videoStream() string {
	req, err := http.NewRequest(http.MethodGet, d.base+"/status", nil)
	if err != nil {
		return ""
	}
	if d.token != "" {
		req.Header.Set("Authorization", "Bearer "+d.token)
	}
	resp, err := (&http.Client{Timeout: 5 * time.Second}).Do(req)
	if err != nil {
		return ""
	}
	defer resp.Body.Close()
	var st struct {
		Video json.RawMessage `json:"video"`
	}
	if resp.StatusCode != http.StatusOK || json.NewDecoder(resp.Body).Decode(&st) != nil {
		return ""
	}
	return compactJSON(st.Video)
}

func (d *daemonSender) RunLine(line string) (string, error) {
	req, err := http.NewRequest(http.MethodPost, d.base+"/send", strings.NewReader(line))
	if err != nil {
		return "", err
	}
	if d.token != "" {
		req.Header.Set("Authorization", "Bearer "+d.token)
	}
	resp, err := (&http.Client{Timeout: sendTimeout + 15*time.Second}).Do(req)
	if err != nil {
		return "", fmt.Errorf("hid daemon /send failed: %w", err)
	}
	defer resp.Body.Close()
	body, _ := io.ReadAll(resp.Body)
	text := strings.TrimSpace(string(body))
	if resp.StatusCode != http.StatusOK {
		if text == "" {
			text = resp.Status
		}
		return "", errors.New(text)
	}
	return text, nil
}

func cmdRun(g globals, args []string, stdin io.Reader, stdout io.Writer) error {
	fs := flag.NewFlagSet("run", flag.ContinueOnError)
	fs.SetOutput(io.Discard)
	delayMS := fs.Int("delay-ms", 0, "")
	// Flags may follow the file name, so split positionals from flags by hand.
	var file string
	var flagArgs []string
	for i := 0; i < len(args); i++ {
		if strings.HasPrefix(args[i], "-") && args[i] != "-" {
			flagArgs = append(flagArgs, args[i])
			if !strings.Contains(args[i], "=") && i+1 < len(args) {
				i++
				flagArgs = append(flagArgs, args[i])
			}
		} else if file == "" {
			file = args[i]
		} else {
			return &usageError{"run takes one file"}
		}
	}
	if err := fs.Parse(flagArgs); err != nil {
		return &usageError{err.Error()}
	}
	if file == "" {
		return &usageError{"run needs a file (or - for stdin)"}
	}
	var text []byte
	var err error
	if file == "-" {
		text, err = io.ReadAll(stdin)
	} else {
		text, err = os.ReadFile(file)
	}
	if err != nil {
		return fmt.Errorf("%s: %w", file, err)
	}
	steps, err := parseSequence(string(text))
	if err != nil {
		return err
	}
	tx, err := openSender(g)
	if err != nil {
		return err
	}
	defer tx.Close()
	sent := 0
	for _, st := range steps {
		if st.isDelay {
			time.Sleep(time.Duration(st.delay * float64(time.Second)))
			continue
		}
		if _, err := tx.RunLine(st.cmd); err != nil {
			return err
		}
		sent++
		if *delayMS > 0 {
			time.Sleep(time.Duration(*delayMS) * time.Millisecond)
		}
	}
	fmt.Fprintf(stdout, "OK (%d commands)\n", sent)
	return nil
}

func cmdStop(g globals, stdout io.Writer) error {
	d := discover(g.target)
	if d == nil {
		fmt.Fprintln(stdout, "no hid daemon running")
		return nil
	}
	if err := requestStop(d); err != nil {
		return err
	}
	fmt.Fprintf(stdout, "hid daemon (pid %d) stopping\n", d.PID)
	return nil
}

// requestStop shuts the daemon down through its token-protected POST /stop.
// It never signals d.PID: a discovery file outlives a daemon that did not exit
// cleanly, and once the kernel reuses the pid the record names an unrelated
// process. Only the token proves the request reached the daemon that wrote it.
func requestStop(d *Discovery) error {
	if d.Token == "" {
		return errors.New("daemon has no token; use `panio daemons stop hid` to stop the older daemon")
	}
	req, err := http.NewRequest(http.MethodPost, fmt.Sprintf("http://127.0.0.1:%d/stop", d.Port), bytes.NewReader(nil))
	if err != nil {
		return err
	}
	req.Header.Set("Authorization", "Bearer "+d.Token)
	resp, err := (&http.Client{Timeout: 5 * time.Second}).Do(req)
	if err != nil {
		return fmt.Errorf("authenticated shutdown failed: %w", err)
	}
	defer resp.Body.Close()
	if resp.StatusCode != http.StatusOK {
		return fmt.Errorf("authenticated shutdown failed: %s", resp.Status)
	}
	return nil
}

func cmdServe(g globals, args []string) error {
	fs := flag.NewFlagSet("serve", flag.ContinueOnError)
	fs.SetOutput(io.Discard)
	port := fs.Int("port", 0, "")
	ffmpegFlag := fs.String("ffmpeg", "", "")
	if err := fs.Parse(args); err != nil {
		return &usageError{err.Error()}
	}
	if g.device == "" {
		return &usageError{"required argument '--device <host[:port]>' (-d) was not provided"}
	}
	if _, err := splitHost(g.device); err != nil {
		return &usageError{err.Error()}
	}
	initLogging()
	// Fail fast: a daemon started by panio with no password should die with
	// a clear line in its log, not sit there unable to connect.
	pw, err := resolvePassword(g.sources)
	if err != nil {
		return err
	}
	host := g.device
	var video *Video
	ffmpeg, ferr := findFFmpeg(*ffmpegFlag)
	if ferr != nil {
		slog.Warn("video unavailable", "err", ferr)
	}
	owner := NewOwner(host, func(ctx context.Context) (rpcLink, error) {
		// The video track only rides the session when ffmpeg can decode it.
		var vv *Video
		if ok, _ := video.Available(); ok {
			vv = video
		}
		return Dial(ctx, DialConfig{Host: host, Password: pw, Video: vv})
	})
	reason := ""
	if ferr != nil {
		reason = ferr.Error()
	}
	video = NewVideo(ffmpeg, reason, owner)
	owner.SetVideo(video)
	return serveDaemon(host, g.target, *port, owner)
}
