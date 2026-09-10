// Copyright 2024 Google LLC
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

/// Callbacks that the wrapping binary can implement.
///
/// These allow you to do stuff like inject telemetry into the daemon
/// or trigger background processes based on a particular session
/// name (for example you could update and re-build a repository n
/// minutes after your `devserver` session disconnects on the assumption
/// that the user is done for the day).
///
/// Hooks are invoked inline within the daemon's control flow, so
/// you MUST NOT block for extended periods of time. If you need to
/// do work that could block for a while, you should spin up a worker
/// thread and enqueue events so the hooks can be processed async.
///
/// It would be nicer if the hooks took `&mut self`, but they are called
/// from an immutable context and it is nice to avoid the syncronization
/// / interior mutability unless it is required. Users can always get
/// mutable state with a cell / mutex.
///
/// Any errors returned will simply be logged.
///
/// All hooks do nothing by default.
pub trait Hooks {
    /// Optional input processing for one attachment. Runs on that client's
    /// existing input thread, after keybindings. None preserves upstream bytes.
    fn session_input(&self, _session_name: &str) -> Option<Box<dyn SessionInput + Send>> {
        None
    }
    /// Override output restoration for a newly created session. The spool is
    /// owned by the PTY output thread: it sees detached output too, and its
    /// restore bytes precede live output on every reattach. Returning None
    /// preserves the configured upstream restore engine.
    fn session_spool(&self, _session_name: &str) -> Option<Box<dyn crate::SessionSpool + Send>> {
        None
    }

    /// Triggered when a fresh session is created.
    fn on_new_session(&self, _session_name: &str) -> anyhow::Result<()> {
        Ok(())
    }

    /// Triggered when a user connects to an existing session.
    fn on_reattach(&self, _session_name: &str) -> anyhow::Result<()> {
        Ok(())
    }

    /// Triggered when a user tries connects to a session but can't because
    /// there is already a connected client.
    fn on_busy(&self, _session_name: &str) -> anyhow::Result<()> {
        Ok(())
    }

    /// Triggered when the `shpool attach` process hangs up.
    fn on_client_disconnect(&self, _session_name: &str) -> anyhow::Result<()> {
        Ok(())
    }

    /// Triggered when a session closes due to some event on the daemon such
    /// as the shell exiting.
    fn on_shell_disconnect(&self, _session_name: &str) -> anyhow::Result<()> {
        Ok(())
    }
}

/// An input adapter owned by the client-to-PTY thread. A slow operation may
/// wait on this session's input, but must observe `stop` so detach can finish.
pub trait SessionInput {
    /// Process bytes, writing their replacement to the PTY. An empty slice
    /// flushes a partial sequence after an input timeout or EOF.
    fn process(
        &mut self,
        bytes: &[u8],
        output: &mut dyn std::io::Write,
        stop: &std::sync::atomic::AtomicBool,
    ) -> std::io::Result<()>;

    /// Whether a partial sequence needs a short read timeout before flushing.
    fn pending(&self) -> bool {
        false
    }
}
