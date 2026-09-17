//! Private Electron protocol, verified against ChatGPT 26.903.71938 (8576).
//! See docs/codex-desktop-ipc.md before changing any wire constants or decoders.
use agentsmon_core::AgentState;
use serde::{Deserialize, Deserializer};
use serde_json::value::RawValue;

pub(super) const STREAM_VERSION: u32 = 11;
pub(super) const FOLLOW_VERSION: u32 = 1;
pub(super) const OWNER_VERSION: u32 = 1;
pub(super) const MAX_FRAME: usize = 16 * 1024 * 1024;

#[derive(Debug)]
pub(super) struct Fault {
    pub code: &'static str,
    pub phase: &'static str,
    pub detail: String,
    pub alert: bool,
}

impl Fault {
    pub fn protocol(code: &'static str, phase: &'static str, detail: impl Into<String>) -> Self {
        Self {
            code,
            phase,
            detail: detail.into(),
            alert: true,
        }
    }

    pub fn io(phase: &'static str, error: std::io::Error) -> Self {
        Self {
            code: "transport_unavailable",
            phase,
            // Never print arbitrary remote errors or incoming JSON.
            detail: format!(
                "io_kind={:?}; os_error={:?}",
                error.kind(),
                error.raw_os_error()
            ),
            alert: false,
        }
    }

    pub fn timeout(phase: &'static str) -> Self {
        Self {
            code: "response_timeout",
            phase,
            detail: "deadline exceeded".into(),
            alert: phase != "owner",
        }
    }
}

/// RawValue is only a borrowed slice of the current bounded frame. Decode only
/// the appropriate metadata schema AFTER checking the envelope/routing. Never
/// store this slice or deserialize it into Value; most bodies contain content.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct Frame<'a> {
    #[serde(rename = "type")]
    pub kind: &'a str,
    pub method: Option<&'a str>,
    pub request_id: Option<&'a str>,
    pub source_client_id: Option<&'a str>,
    pub version: Option<u32>,
    pub result_type: Option<&'a str>,
    pub error: Option<RemoteError>,
    pub handled_by_client_id: Option<&'a str>,
    #[serde(borrow)]
    pub result: Option<&'a RawValue>,
    #[serde(borrow)]
    pub params: Option<&'a RawValue>,
}

#[derive(Deserialize, PartialEq, Eq)]
pub(super) enum RemoteError {
    #[serde(rename = "no-client-found")]
    NoClient,
    #[serde(other)]
    Other,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct Initialized {
    pub client_id: String,
}

// No turns, titles, messages, requests, tool arguments/results, or previews.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct SnapshotRoute<'a> {
    pub conversation_id: &'a str,
    pub host_id: &'a str,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct Snapshot<'a> {
    pub conversation_id: &'a str,
    pub host_id: &'a str,
    pub change: Change<'a>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct Change<'a> {
    #[serde(rename = "type")]
    pub kind: &'a str,
    pub conversation_state: Option<ConversationStatus<'a>>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct ConversationStatus<'a> {
    pub id: &'a str,
    pub thread_runtime_status: RuntimeStatus,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct RuntimeStatus {
    #[serde(rename = "type")]
    kind: StatusKind,
    #[serde(default, deserialize_with = "limited_flags")]
    active_flags: Option<Vec<ActiveFlag>>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
enum StatusKind {
    Active,
    Idle,
    NotLoaded,
    SystemError,
    #[serde(other)]
    Unknown,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
enum ActiveFlag {
    WaitingOnApproval,
    WaitingOnUserInput,
    #[serde(other)]
    Unknown,
}

fn limited_flags<'de, D: Deserializer<'de>>(
    deserializer: D,
) -> Result<Option<Vec<ActiveFlag>>, D::Error> {
    struct Flags;
    impl<'de> serde::de::Visitor<'de> for Flags {
        type Value = Option<Vec<ActiveFlag>>;
        fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
            f.write_str("bounded runtime flags array")
        }
        fn visit_seq<A: serde::de::SeqAccess<'de>>(
            self,
            mut seq: A,
        ) -> Result<Self::Value, A::Error> {
            let mut flags = Vec::new();
            while let Some(flag) = seq.next_element()? {
                if flags.len() == 16 {
                    return Err(serde::de::Error::custom("runtime flags limit"));
                }
                flags.push(flag);
            }
            Ok(Some(flags))
        }
    }
    deserializer.deserialize_seq(Flags)
}

impl RuntimeStatus {
    pub fn state(&self) -> Result<Option<AgentState>, Fault> {
        match self.kind {
            StatusKind::Active => {
                let flags = self.active_flags.as_ref().ok_or_else(|| {
                    Fault::protocol("missing_active_flags", "snapshot", "activeFlags absent")
                })?;
                if flags.iter().any(|f| matches!(f, ActiveFlag::Unknown)) {
                    return Err(Fault::protocol(
                        "unknown_active_flag",
                        "snapshot",
                        "unrecognized activeFlags entry (value omitted)",
                    ));
                }
                Ok(Some(if flags.is_empty() {
                    AgentState::Active
                } else {
                    AgentState::Waiting
                }))
            }
            StatusKind::Idle => Ok(Some(AgentState::Idle)),
            StatusKind::NotLoaded | StatusKind::SystemError => Ok(None),
            StatusKind::Unknown => Err(Fault::protocol(
                "unknown_runtime_status",
                "snapshot",
                "unrecognized status type (value omitted)",
            )),
        }
    }
}

pub(super) fn decode<'a, T: Deserialize<'a>>(
    bytes: &'a [u8],
    phase: &'static str,
) -> Result<T, Fault> {
    serde_json::from_slice(bytes).map_err(|error| {
        // serde errors can embed input values: retain only category and location.
        Fault::protocol(
            "schema_changed",
            phase,
            format!(
                "json_category={:?}; line={}; column={}",
                error.classify(),
                error.line(),
                error.column()
            ),
        )
    })
}

pub(super) fn valid_id(id: &str) -> bool {
    id.len() == 36
        && id.bytes().enumerate().all(|(i, b)| {
            if matches!(i, 8 | 13 | 18 | 23) {
                b == b'-'
            } else {
                b.is_ascii_hexdigit()
            }
        })
}
