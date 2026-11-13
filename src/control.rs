//! Explicit agent/human input arbitration.
//!
//! The agent owns a session by default.  A viewer (or a person) claims `human`
//! control before forwarding host input, and the server refuses pointer,
//! keyboard, and accessibility actions until control is released.  Control
//! lives in its own small file so the server and a viewer can update it without
//! racing on the session state JSON.  A claim whose process has exited is
//! treated as stale and ignored, so a crashed viewer cannot lock an agent out.

use crate::state::State;
use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::time::Duration;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Owner {
    #[default]
    Agent,
    Human,
}

impl Owner {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Agent => "agent",
            Self::Human => "human",
        }
    }
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Control {
    #[serde(default)]
    pub owner: Owner,
    /// Process that claimed human control; 0 for the agent.
    #[serde(default)]
    pub pid: u32,
    /// Unix seconds when control last changed.
    #[serde(default)]
    pub since: u64,
}

impl Control {
    pub fn agent() -> Self {
        Self {
            owner: Owner::Agent,
            pid: 0,
            since: crate::state::now(),
        }
    }
}

pub fn path(session_id: &str) -> PathBuf {
    crate::state::sessions_dir().join(format!("{session_id}.control.json"))
}

fn write_atomic(path: &Path, control: &Control) -> Result<()> {
    if let Some(parent) = path.parent() {
        crate::env::create_private_dir(parent)
            .with_context(|| format!("cannot create {}", parent.display()))?;
    }
    let temp = path.with_extension("control.json.tmp");
    std::fs::write(&temp, serde_json::to_vec_pretty(control)?)
        .with_context(|| format!("cannot write {}", temp.display()))?;
    std::fs::rename(&temp, path).with_context(|| format!("cannot publish {}", path.display()))?;
    Ok(())
}

pub fn read(session_id: &str) -> Control {
    std::fs::read(path(session_id))
        .ok()
        .and_then(|bytes| serde_json::from_slice(&bytes).ok())
        .unwrap_or_default()
}

pub fn write(session_id: &str, control: &Control) -> Result<()> {
    write_atomic(&path(session_id), control)
}

/// Read control, clearing a human claim whose process is gone.
pub fn effective(session_id: &str) -> Control {
    let mut control = read(session_id);
    if control.owner == Owner::Human && control.pid != 0 && !crate::proc::alive(control.pid) {
        control = Control::agent();
        let _ = write(session_id, &control);
    }
    control
}

pub fn claim(session_id: &str, owner: Owner, pid: u32) -> Result<Control> {
    let current = effective(session_id);
    if current.owner == Owner::Human
        && owner == Owner::Human
        && current.pid != 0
        && current.pid != pid
        && crate::proc::alive(current.pid)
    {
        bail!(
            "session {session_id:?} is already under human control (pid {} since {})",
            current.pid,
            current.since
        );
    }
    let next = Control {
        owner,
        pid: if owner == Owner::Human { pid } else { 0 },
        since: crate::state::now(),
    };
    write(session_id, &next)?;
    Ok(next)
}

/// Release only our own claim; another viewer's claim is left alone.
pub fn release(session_id: &str, pid: u32) -> Result<Control> {
    let current = effective(session_id);
    if current.owner == Owner::Human && current.pid != 0 && current.pid != pid {
        return Ok(current);
    }
    let next = Control::agent();
    write(session_id, &next)?;
    Ok(next)
}

pub fn require_agent(session_id: &str) -> Result<()> {
    let control = effective(session_id);
    if control.owner == Owner::Human {
        bail!(
            "session {session_id:?} is under human control (pid {} since {}); \
             input and accessibility actions are refused until the viewer releases it",
            control.pid,
            control.since
        );
    }
    Ok(())
}

/// Block until `owner` has control or the timeout expires.
pub async fn wait(session_id: &str, owner: Owner, timeout: Duration, poll: Duration) -> Control {
    let started = std::time::Instant::now();
    loop {
        let control = effective(session_id);
        if control.owner == owner || started.elapsed() >= timeout {
            return control;
        }
        tokio::time::sleep(poll).await;
    }
}

pub fn clear(session_id: &str) {
    let _ = std::fs::remove_file(path(session_id));
}

/// Control snapshot for a state file or status response.
pub fn state_fields(session_id: &str) -> (String, Option<u32>, Option<u64>) {
    let control = effective(session_id);
    match control.owner {
        Owner::Agent => (Owner::Agent.as_str().to_string(), None, None),
        Owner::Human => (
            Owner::Human.as_str().to_string(),
            Some(control.pid),
            Some(control.since),
        ),
    }
}

/// Reusable snapshot from a `State` without another file read.
pub fn from_state(state: &State) -> Control {
    Control {
        owner: match state.control.as_str() {
            "human" => Owner::Human,
            _ => Owner::Agent,
        },
        pid: state.control_pid.unwrap_or(0),
        since: state.control_since.unwrap_or(0),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn owner_round_trips() {
        let encoded = serde_json::to_string(&Owner::Human).unwrap();
        assert_eq!(encoded, "\"human\"");
        let decoded: Owner = serde_json::from_str(&encoded).unwrap();
        assert_eq!(decoded, Owner::Human);
    }

    #[test]
    fn default_control_is_agent() {
        let control = Control::default();
        assert_eq!(control.owner, Owner::Agent);
        assert_eq!(control.pid, 0);
    }

    #[test]
    fn human_claim_blocks_the_agent_until_released() {
        let id = format!("control-test-{}", std::process::id());
        clear(&id);
        let pid = std::process::id();
        claim(&id, Owner::Human, pid).unwrap();
        assert_eq!(effective(&id).owner, Owner::Human);
        assert!(require_agent(&id).is_err());
        release(&id, pid).unwrap();
        assert_eq!(effective(&id).owner, Owner::Agent);
        assert!(require_agent(&id).is_ok());
        clear(&id);
    }

    #[test]
    fn a_dead_claimer_is_ignored() {
        let id = format!("control-stale-{}", std::process::id());
        clear(&id);
        write(
            &id,
            &Control {
                owner: Owner::Human,
                pid: 999_999,
                since: 1,
            },
        )
        .unwrap();
        assert_eq!(effective(&id).owner, Owner::Agent);
        assert!(require_agent(&id).is_ok());
        clear(&id);
    }
}
