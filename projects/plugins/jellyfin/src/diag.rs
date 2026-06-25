//! Transcode-diagnosis types for `jellyfin.transcode_health`.
//!
//! These mirror the slice of Jellyfin's `SessionInfo` / `TranscodingInfo`
//! the diagnosis needs. They are *not* the progenitor-generated client types:
//! progenitor's types don't derive `JsonSchema`, so they cannot cross an
//! `#[orca_tool]` boundary. Modeling the diagnosis surface as its own typed
//! structs (deriving serde + schemars) is the canonical pattern — the tool
//! deserializes the upstream `/Sessions` JSON into these.
//!
//! Field availability note (spec `info.version` 12.0.0): `TranscodingInfo`
//! exposes `HardwareAccelerationType`, `IsVideoDirect`, `IsAudioDirect`, and
//! `TranscodeReasons`. It does NOT carry `VideoDecoderIsHardware` /
//! `VideoEncoderIsHardware` in this version, so those are not modeled —
//! `HardwareAccelerationType` is the authoritative HW-vs-software signal:
//! `none` (or an absent `TranscodingInfo`) means a SOFTWARE transcode.

use plugin_toolkit::prelude::*;

/// One active playback session, narrowed to the transcode-health fields.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub struct RawSession {
    #[serde(default)]
    pub id: Option<String>,
    #[serde(default)]
    pub user_name: Option<String>,
    #[serde(default)]
    pub client: Option<String>,
    #[serde(default)]
    pub device_name: Option<String>,
    #[serde(default)]
    pub now_playing_item: Option<RawNowPlaying>,
    #[serde(default)]
    pub transcoding_info: Option<RawTranscodingInfo>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub struct RawNowPlaying {
    #[serde(default)]
    pub name: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub struct RawTranscodingInfo {
    #[serde(default)]
    pub hardware_acceleration_type: Option<String>,
    #[serde(default)]
    pub is_video_direct: Option<bool>,
    #[serde(default)]
    pub video_codec: Option<String>,
    #[serde(default)]
    pub audio_codec: Option<String>,
    #[serde(default)]
    pub transcode_reasons: Option<Vec<String>>,
}

/// Per-session transcode health, as returned by `jellyfin.transcode_health`.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct SessionTranscodeHealth {
    /// Session id, if the upstream reported one.
    pub session_id: Option<String>,
    /// Logged-in user for the session.
    pub user_name: Option<String>,
    /// Client app (e.g. the player) driving the session.
    pub client: Option<String>,
    /// Device name.
    pub device_name: Option<String>,
    /// Title currently playing, if any.
    pub now_playing: Option<String>,
    /// True when this session is actively transcoding (has `TranscodingInfo`).
    pub is_transcoding: bool,
    /// The hardware acceleration type reported by Jellyfin
    /// (`qsv`, `nvenc`, `vaapi`, …). `None` or `"none"` means software.
    pub hardware_acceleration_type: Option<String>,
    /// **Core flag.** True when this session is transcoding on the CPU
    /// (software fallback) rather than a hardware encoder/decoder.
    pub software_fallback: bool,
    /// True when the video stream is passed through without transcoding.
    pub is_video_direct: Option<bool>,
    /// Negotiated video codec for the transcode, if transcoding.
    pub video_codec: Option<String>,
    /// Negotiated audio codec for the transcode, if transcoding.
    pub audio_codec: Option<String>,
    /// Why Jellyfin chose to transcode (codec/bitrate/container mismatch, …).
    pub transcode_reasons: Vec<String>,
}

impl SessionTranscodeHealth {
    /// Classify a raw upstream session. A session with no `TranscodingInfo`
    /// is direct-playing (not transcoding, no fallback). A transcoding session
    /// whose `HardwareAccelerationType` is absent or `"none"` is a software
    /// fallback — the condition operators chase.
    pub fn from_raw(s: RawSession) -> Self {
        let now_playing = s.now_playing_item.and_then(|n| n.name);
        match s.transcoding_info {
            None => Self {
                session_id: s.id,
                user_name: s.user_name,
                client: s.client,
                device_name: s.device_name,
                now_playing,
                is_transcoding: false,
                hardware_acceleration_type: None,
                software_fallback: false,
                is_video_direct: None,
                video_codec: None,
                audio_codec: None,
                transcode_reasons: Vec::new(),
            },
            Some(t) => {
                let hw = t.hardware_acceleration_type.filter(|v| !v.is_empty());
                let software_fallback = match &hw {
                    None => true,
                    Some(v) => v.eq_ignore_ascii_case("none"),
                };
                Self {
                    session_id: s.id,
                    user_name: s.user_name,
                    client: s.client,
                    device_name: s.device_name,
                    now_playing,
                    is_transcoding: true,
                    hardware_acceleration_type: hw,
                    software_fallback,
                    is_video_direct: t.is_video_direct,
                    video_codec: t.video_codec,
                    audio_codec: t.audio_codec,
                    transcode_reasons: t.transcode_reasons.unwrap_or_default(),
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn raw(hw: Option<&str>, transcoding: bool) -> RawSession {
        RawSession {
            id: Some("s1".into()),
            user_name: Some("u".into()),
            client: Some("c".into()),
            device_name: Some("d".into()),
            now_playing_item: Some(RawNowPlaying {
                name: Some("Movie".into()),
            }),
            transcoding_info: transcoding.then(|| RawTranscodingInfo {
                hardware_acceleration_type: hw.map(str::to_string),
                is_video_direct: Some(false),
                video_codec: Some("h264".into()),
                audio_codec: Some("aac".into()),
                transcode_reasons: Some(vec!["VideoCodecNotSupported".into()]),
            }),
        }
    }

    #[test]
    fn hardware_transcode_is_not_fallback() {
        let h = SessionTranscodeHealth::from_raw(raw(Some("qsv"), true));
        assert!(h.is_transcoding);
        assert!(!h.software_fallback);
        assert_eq!(h.hardware_acceleration_type.as_deref(), Some("qsv"));
    }

    #[test]
    fn none_hwaccel_is_software_fallback() {
        let h = SessionTranscodeHealth::from_raw(raw(Some("none"), true));
        assert!(h.software_fallback);
    }

    #[test]
    fn absent_hwaccel_on_transcode_is_software_fallback() {
        let h = SessionTranscodeHealth::from_raw(raw(None, true));
        assert!(h.is_transcoding);
        assert!(h.software_fallback);
        assert_eq!(h.transcode_reasons, vec!["VideoCodecNotSupported"]);
    }

    #[test]
    fn direct_play_is_neither_transcoding_nor_fallback() {
        let h = SessionTranscodeHealth::from_raw(raw(None, false));
        assert!(!h.is_transcoding);
        assert!(!h.software_fallback);
        assert_eq!(h.now_playing.as_deref(), Some("Movie"));
    }
}
