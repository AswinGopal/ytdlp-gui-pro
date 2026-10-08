use std::path::PathBuf;

use anyhow::{anyhow, bail, ensure};
use serde::{Deserialize, Serialize};

use crate::util;

pub type Result<T> = anyhow::Result<T>;

/// Payload emitted on every progress tick of a running download.
///
/// `pct`, `downloaded` and `total` describe the file currently being fetched,
/// except for a download that fetches several streams (video and audio are
/// separate files): there they describe the whole job, so the bar runs once
/// from 0 to 100 instead of once per stream (see `DlOpts::streams`).
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct ProgressInfo {
    /// Download completion, 0.0 – 100.0.
    pub pct: f64,
    /// Transfer speed in bytes per second. `None` while yt-dlp has not
    /// reported one yet, and always `None` for ffmpeg-driven section
    /// downloads, which expose no throughput figure.
    pub speed: Option<f64>,
    /// Bytes written so far for the file currently being fetched, or for all
    /// the job's streams together.
    pub downloaded: Option<u64>,
    /// Total size of that file (or of all the streams), when known or
    /// estimated.
    pub total: Option<u64>,
    /// `true` when `total` is an estimate (fragmented streams, ffmpeg
    /// sections, a stream yt-dlp has not started yet) rather than a size
    /// reported by the server.
    pub total_estimated: bool,
}

/// A stream a full-video download is expected to fetch, as the probe sees it.
/// It gives the whole-job progress of a multi-stream download its total
/// before yt-dlp has reported the size of every stream.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExpectedStream {
    pub format_id: String,
    /// The probe's size for the stream; `None` when it reports none.
    pub bytes: Option<u64>,
    /// `true` when `bytes` is exact, `false` when it is an estimate.
    pub exact: bool,
}

/// Everything `yt::run` needs for one yt-dlp invocation.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct DlOpts {
    pub url: String,
    pub format: Option<String>,
    pub sections: Option<String>,
    pub extra: Vec<String>,
    pub output_name: Option<String>,
    /// Where finished files land (`-P home:`).
    pub dest_dir: PathBuf,
    /// Where `.part` files and other intermediates live (`-P temp:`). Keeping
    /// them in a per-job directory is what lets stop / remove delete exactly
    /// one job's leftovers without guessing at file names.
    pub temp_dir: PathBuf,
    /// The streams a full-video download is expected to fetch. With two or
    /// more, progress is reported for the whole job rather than per stream:
    /// yt-dlp downloads video and audio as separate files and restarts its
    /// counters for each. Empty for clips and chapters, which one ffmpeg call
    /// fetches as a whole, and whenever the probe cannot say.
    #[serde(default)]
    pub streams: Vec<ExpectedStream>,
}

/// Wire names match what the frontend already sends (`"mp3"`, `"1080p"`, …).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Preset {
    Mp3,
    M4a,
    #[serde(rename = "1080p")]
    Video1080p,
    Best,
}

/// What to download, independent of how much of the video to download.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", content = "value", rename_all = "snake_case")]
pub enum Quality {
    Preset(Preset),
    /// A format selection, in one of these shapes:
    ///
    /// * one combined format, downloaded as it is;
    /// * one video-only format, which the download completes with the audio
    ///   stream that matches its container (see `Meta::format_selector`);
    /// * one video-only plus one audio-only format, honoured as given (older
    ///   rows and history entries carry these);
    /// * one audio-only format, downloaded as it is (also from older rows).
    Formats(Vec<String>),
}

/// How much of the video to download, independent of quality.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Scope {
    Full,
    /// `HH:MM:SS` bounds once validated; an empty `start` means the beginning.
    Timeline { start: String, end: String },
    /// Zero-based indices into the video's chapter list.
    Chapters { selected: Vec<usize> },
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct JobSpec {
    pub quality: Quality,
    pub scope: Scope,
}

impl Default for JobSpec {
    fn default() -> Self {
        Self { quality: Quality::Preset(Preset::Best), scope: Scope::Full }
    }
}

impl Quality {
    /// Rejects selections yt-dlp could not honour for this video: unknown IDs,
    /// selector metacharacters, or a pairing other than one combined format /
    /// one video-only plus one audio-only format.
    pub fn validated(&self, formats: &[FormatInfo]) -> Result<Quality> {
        let Quality::Formats(ids) = self else {
            return Ok(self.clone());
        };
        let ids: Vec<String> = ids.iter().map(|id| id.trim().to_owned()).collect();

        let kinds = ids
            .iter()
            .map(|id| {
                ensure!(
                    !id.is_empty() && !id.contains(|c: char| c.is_whitespace() || "+/,[]()".contains(c)),
                    "Format ID {id:?} cannot be used in a format selection."
                );
                formats
                    .iter()
                    .find(|f| f.format_id == *id)
                    .map(FormatInfo::kind)
                    .ok_or_else(|| anyhow!("Format {id} is not available for this video."))
            })
            .collect::<Result<Vec<_>>>()?;

        match kinds.as_slice() {
            [StreamKind::Unknown] => bail!("Format {} carries neither video nor audio.", ids[0]),
            [_] => {}
            [StreamKind::VideoOnly, StreamKind::AudioOnly]
            | [StreamKind::AudioOnly, StreamKind::VideoOnly] => {}
            [_, _] => bail!("Pick one combined format, or one video-only plus one audio-only format."),
            _ => bail!("Select one or two formats."),
        }
        Ok(Quality::Formats(ids))
    }
}

impl Scope {
    /// Validates against the video's chapter count and duration (when the
    /// extractor reports one) and returns the normalised scope: timestamps as
    /// `HH:MM:SS`, chapter indices sorted and de-duplicated.
    pub fn validated(&self, chapter_count: usize, duration: Option<f64>) -> Result<Scope> {
        match self {
            Scope::Full => Ok(Scope::Full),
            Scope::Timeline { start, end } => {
                let (start, end) = (start.trim(), end.trim());
                ensure!(!end.is_empty(), "Enter an end time.");
                let start_secs = if start.is_empty() {
                    0
                } else {
                    util::parse_clock(start)
                        .ok_or_else(|| anyhow!("Start time — use SS, MM:SS, or HH:MM:SS."))?
                };
                let end_secs = util::parse_clock(end)
                    .ok_or_else(|| anyhow!("End time — use SS, MM:SS, or HH:MM:SS."))?;
                ensure!(end_secs > start_secs, "End time must be after start time.");
                if let Some(d) = duration {
                    ensure!(
                        f64::from(end_secs) <= d,
                        "End time exceeds video length ({}).",
                        util::seconds_to_hms(d)
                    );
                }
                Ok(Scope::Timeline {
                    start: util::seconds_to_hms(f64::from(start_secs)),
                    end: util::seconds_to_hms(f64::from(end_secs)),
                })
            }
            Scope::Chapters { selected } => {
                ensure!(!selected.is_empty(), "Select at least one chapter.");
                ensure!(
                    selected.iter().all(|&i| i < chapter_count),
                    "A selected chapter does not exist for this video."
                );
                let mut selected = selected.clone();
                selected.sort_unstable();
                selected.dedup();
                Ok(Scope::Chapters { selected })
            }
        }
    }
}

impl JobSpec {
    pub fn validated(
        &self,
        formats: &[FormatInfo],
        chapter_count: usize,
        duration: Option<f64>,
    ) -> Result<JobSpec> {
        Ok(JobSpec {
            quality: self.quality.validated(formats)?,
            scope: self.scope.validated(chapter_count, duration)?,
        })
    }

    /// Like `validated`, but repairs instead of failing: an invalid quality
    /// falls back to `fallback`, an invalid scope to the full video. Used after
    /// a probe so a spec carried over from history (or an older probe) never
    /// outlives the metadata it was written against.
    pub fn reconciled(
        &self,
        formats: &[FormatInfo],
        chapter_count: usize,
        duration: Option<f64>,
        fallback: Preset,
    ) -> JobSpec {
        JobSpec {
            quality: self
                .quality
                .validated(formats)
                .unwrap_or(Quality::Preset(fallback)),
            scope: self.scope.validated(chapter_count, duration).unwrap_or(Scope::Full),
        }
    }
}

/// Deserializes a `String` field that yt-dlp may emit as either a JSON string
/// or an explicit `null`.  `#[serde(default)]` alone only fires when the key
/// is *absent*; when the key is present with value `null` serde still tries
/// to deserialize `null` as `String`, which fails.  Going through
/// `Option<String>` first accepts both representations.
pub(crate) fn deser_string<'de, D: serde::Deserializer<'de>>(
    d: D,
) -> std::result::Result<String, D::Error> {
    Option::<String>::deserialize(d).map(|o| o.unwrap_or_default())
}

/// Deserializes a codec field (`vcodec` / `acodec`) that yt-dlp may emit as a
/// JSON string, an explicit `null`, or omit entirely.
///
/// Three distinct states need to be preserved:
///
/// * **`"none"`** — yt-dlp explicitly sets this string to signal "this stream
///   carries no track of this type" (e.g. a DASH video-only shard has
///   `"acodec": "none"`).  The probe's filter uses `!= "none"` to drop streams
///   that carry neither track, so this sentinel must round-trip unchanged.
///
/// * **`"unknown"`** — the field is present with value `null`, or is absent
///   altogether.  Both cases arise from the generic extractor, which builds
///   format dicts from bare URLs and never populates codec metadata.
///   Collapsing these to `"none"` (the old behaviour) caused every such format
///   to be discarded by the filter, producing an empty table.  `"unknown"` is
///   the conventional yt-dlp display value for undetected codecs and passes
///   the `!= "none"` guard correctly.
///
/// * **any other string** — a real codec identifier; passed through as-is.
fn deser_codec<'de, D: serde::Deserializer<'de>>(d: D) -> std::result::Result<String, D::Error> {
    Option::<String>::deserialize(d).map(|o| o.unwrap_or_else(|| "unknown".to_string()))
}

/// Default for absent `vcodec` / `acodec` keys.  Mirrors `deser_codec`'s
/// null-branch: both absent and null mean "codec unknown, not absent by
/// design".  See `deser_codec` for the full three-state rationale.
fn codec_unknown() -> String {
    "unknown".to_string()
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StreamKind {
    Muxed,
    VideoOnly,
    AudioOnly,
    /// Neither track present; the probe filters these out.
    Unknown,
}

/// A single downloadable stream as reported by yt-dlp.
///
/// All numeric fields are `Option<f64>` rather than narrower integer types.
/// yt-dlp has ~1800 extractors, and the JSON shape of "the same" field can
/// vary slightly between them. `f64` deserializes from any JSON number
/// without risk of a hard failure, and the frontend doesn't care about the
/// int/float distinction for display purposes.
///
/// String fields that may arrive as explicit JSON `null` (not merely absent)
/// use `deserialize_with` in addition to `default` so that both the missing-
/// key and the null-value cases are handled without a hard deserialize error.
///
/// `size_bytes` / `size_exact` are not yt-dlp fields: the probe derives them
/// once (see `resolve_size`) and they round-trip through the queue database.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct FormatInfo {
    pub format_id: String,
    #[serde(default, deserialize_with = "deser_string")]
    pub ext: String,
    #[serde(default, deserialize_with = "deser_string")]
    pub resolution: String,
    #[serde(default)]
    pub width: Option<f64>,
    #[serde(default)]
    pub height: Option<f64>,
    #[serde(default)]
    pub fps: Option<f64>,
    #[serde(default = "codec_unknown", deserialize_with = "deser_codec")]
    pub vcodec: String,
    #[serde(default = "codec_unknown", deserialize_with = "deser_codec")]
    pub acodec: String,
    #[serde(default)]
    pub tbr: Option<f64>,
    #[serde(default)]
    pub vbr: Option<f64>,
    #[serde(default)]
    pub abr: Option<f64>,
    #[serde(default)]
    pub filesize: Option<f64>,
    #[serde(default)]
    pub filesize_approx: Option<f64>,
    #[serde(default)]
    pub format_note: Option<String>,
    #[serde(default)]
    pub language: Option<String>,
    #[serde(default)]
    pub dynamic_range: Option<String>,
    #[serde(default, deserialize_with = "deser_string")]
    pub protocol: String,
    #[serde(default)]
    pub size_bytes: Option<u64>,
    #[serde(default)]
    pub size_exact: bool,
}

impl FormatInfo {
    pub fn has_video(&self) -> bool {
        self.vcodec != "none"
    }

    pub fn has_audio(&self) -> bool {
        self.acodec != "none"
    }

    pub fn kind(&self) -> StreamKind {
        match (self.has_video(), self.has_audio()) {
            (true, true) => StreamKind::Muxed,
            (true, false) => StreamKind::VideoOnly,
            (false, true) => StreamKind::AudioOnly,
            (false, false) => StreamKind::Unknown,
        }
    }

    /// Fills `size_bytes` / `size_exact` from the best figure available:
    /// the exact `filesize`, else `filesize_approx`, else bitrate × duration.
    /// Only the first is exact.
    pub(crate) fn resolve_size(&mut self, duration: Option<f64>) {
        fn positive(v: Option<f64>) -> Option<f64> {
            v.filter(|x| x.is_finite() && *x > 0.0)
        }

        let resolved = if let Some(b) = positive(self.filesize) {
            Some((b, true))
        } else if let Some(b) = positive(self.filesize_approx) {
            Some((b, false))
        } else if let (Some(tbr), Some(d)) = (positive(self.tbr), positive(duration)) {
            // tbr is kilobits per second.
            Some((tbr * 1000.0 / 8.0 * d, false))
        } else {
            None
        };

        (self.size_bytes, self.size_exact) = match resolved {
            Some((bytes, exact)) => (Some(bytes as u64), exact),
            None => (None, false),
        };
    }
}

#[cfg(test)]
pub(crate) mod test_support {
    use super::*;

    pub fn fmt(id: &str, vcodec: &str, acodec: &str) -> FormatInfo {
        serde_json::from_value(serde_json::json!({
            "format_id": id, "vcodec": vcodec, "acodec": acodec
        }))
        .unwrap()
    }
}

#[cfg(test)]
mod tests {
    use super::test_support::fmt;
    use super::*;

    fn formats() -> Vec<FormatInfo> {
        vec![fmt("18", "avc1", "mp4a"), fmt("137", "avc1", "none"), fmt("140", "none", "mp4a"), fmt("251", "none", "opus")]
    }

    #[test]
    fn quality_pairing_rules() {
        let f = formats();
        let ok = |ids: &[&str]| {
            Quality::Formats(ids.iter().map(|s| s.to_string()).collect()).validated(&f).is_ok()
        };
        assert!(ok(&["18"]));
        assert!(ok(&["137"]), "a lone video-only pick is valid: audio is added at download time");
        assert!(ok(&["140"]), "a lone audio-only format stays valid for older rows");
        assert!(ok(&["137", "140"]));
        assert!(ok(&["251", "137"]));
        assert!(!ok(&["18", "140"]));
        assert!(!ok(&["137", "137"]));
        assert!(!ok(&["140", "251"]));
        assert!(!ok(&["999"]));
        assert!(!ok(&[]));
        assert!(!ok(&["137", "140", "251"]));
    }

    #[test]
    fn scope_timeline_is_normalised_and_bounded() {
        let s = Scope::Timeline { start: "".into(), end: "1:30".into() };
        assert_eq!(
            s.validated(0, Some(120.0)).unwrap(),
            Scope::Timeline { start: "00:00:00".into(), end: "00:01:30".into() }
        );
        let late = Scope::Timeline { start: "0:10".into(), end: "3:00".into() };
        assert!(late.validated(0, Some(120.0)).is_err());
        assert!(late.validated(0, None).is_ok());
        let backwards = Scope::Timeline { start: "2:00".into(), end: "1:00".into() };
        assert!(backwards.validated(0, None).is_err());
        let no_end = Scope::Timeline { start: "0:10".into(), end: " ".into() };
        assert!(no_end.validated(0, None).is_err());
    }

    #[test]
    fn scope_chapters_are_sorted_deduped_and_bounded() {
        let s = Scope::Chapters { selected: vec![2, 0, 2] };
        assert_eq!(s.validated(3, None).unwrap(), Scope::Chapters { selected: vec![0, 2] });
        assert!(s.validated(2, None).is_err());
        assert!(Scope::Chapters { selected: vec![] }.validated(3, None).is_err());
    }

    #[test]
    fn reconcile_repairs_each_half_independently() {
        let spec = JobSpec {
            quality: Quality::Formats(vec!["gone".into()]),
            scope: Scope::Timeline { start: "0:00".into(), end: "0:30".into() },
        };
        let fixed = spec.reconciled(&formats(), 0, Some(60.0), Preset::Mp3);
        assert_eq!(fixed.quality, Quality::Preset(Preset::Mp3));
        assert!(matches!(fixed.scope, Scope::Timeline { .. }));
    }

    #[test]
    fn wire_format_is_stable() {
        let spec = JobSpec {
            quality: Quality::Preset(Preset::Video1080p),
            scope: Scope::Chapters { selected: vec![1] },
        };
        let json = serde_json::to_value(&spec).unwrap();
        assert_eq!(json["quality"]["kind"], "preset");
        assert_eq!(json["quality"]["value"], "1080p");
        assert_eq!(json["scope"]["kind"], "chapters");
        let back: JobSpec = serde_json::from_value(json).unwrap();
        assert_eq!(back, spec);
    }

    #[test]
    fn size_resolution_order() {
        let mut f = fmt("1", "avc1", "none");
        f.tbr = Some(800.0);
        f.resolve_size(Some(10.0));
        assert_eq!((f.size_bytes, f.size_exact), (Some(1_000_000), false));
        f.filesize_approx = Some(5.0);
        f.resolve_size(Some(10.0));
        assert_eq!((f.size_bytes, f.size_exact), (Some(5), false));
        f.filesize = Some(7.0);
        f.resolve_size(Some(10.0));
        assert_eq!((f.size_bytes, f.size_exact), (Some(7), true));
        let mut none = fmt("2", "avc1", "none");
        none.resolve_size(None);
        assert_eq!(none.size_bytes, None);
    }
}
