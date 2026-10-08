//! One yt-dlp call per queue row that fetches everything the row needs —
//! title, thumbnail, duration, chapters, formats — replacing the four
//! per-field subprocesses the single-form UI spawned.

use std::cmp::Ordering;

use anyhow::{anyhow, ensure};
use serde::{Deserialize, Serialize};

use crate::modes::chapters::Chapter;
use crate::process::{self, CancelToken};
use crate::types::{ExpectedStream, FormatInfo, JobSpec, Preset, Quality, Result, Scope, StreamKind};
use crate::util;

/// `%(.{a,b})j` prints a JSON object holding just those fields. Unlike
/// `--dump-json` it skips the heavyweight rest of the info dict (captions,
/// thumbnails list, heatmap, …). Requires yt-dlp 2023.07.06 or newer.
const PRINT_TEMPLATE: &str = "%(.{title,thumbnail,duration,chapters,formats})j";

/// Everything the queue knows about a video, fetched once per row and then
/// stored with it, so a restored queue never re-probes.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Meta {
    pub title: String,
    pub thumbnail: Option<String>,
    pub duration: Option<f64>,
    pub chapters: Vec<Chapter>,
    pub formats: Vec<FormatInfo>,
}

#[derive(Deserialize)]
struct RawMeta {
    title: Option<String>,
    thumbnail: Option<String>,
    duration: Option<f64>,
    chapters: Option<Vec<Chapter>>,
    formats: Option<Vec<FormatInfo>>,
}

/// A total size in bytes, flagged when it is only an estimate.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
pub struct SizeEstimate {
    pub bytes: u64,
    pub exact: bool,
}

/// The streams a quality selection downloads, in download order.
struct Pick<'a> {
    streams: Vec<&'a FormatInfo>,
    /// `false` when part of the selection is yt-dlp's own choice at download
    /// time, so even exact sizes only add up to an estimate.
    exact_possible: bool,
}

/// Probes `url`. `Ok(None)` means `token` fired and yt-dlp was killed.
pub fn fetch(url: &str, token: &CancelToken) -> Result<Option<Meta>> {
    let mut cmd = util::ytdlp();
    cmd.args([
        "--print",
        PRINT_TEMPLATE,
        "--no-playlist",
        "--skip-download",
        "--no-warnings",
        "-q",
        // Everything after `--` is a positional argument, so a URL that
        // starts with `-` can never be read as an option.
        "--",
        url,
    ]);

    let Some(output) = process::capture(cmd, token)? else {
        return Ok(None);
    };
    ensure!(
        output.status.success(),
        util::extract_ytdlp_error(&String::from_utf8_lossy(&output.stderr), &output.status),
    );
    parse(&output.stdout, url).map(Some)
}

fn parse(stdout: &[u8], url: &str) -> Result<Meta> {
    // Lossy rather than strict: on Windows a non-UTF-8 console code page can
    // mangle a title, and a mangled title beats a failed probe.
    let text = String::from_utf8_lossy(stdout);
    let mut values = serde_json::Deserializer::from_str(&text).into_iter::<RawMeta>();

    let raw = values
        .next()
        .ok_or_else(|| anyhow!("yt-dlp returned no metadata for this URL."))??;
    // `--no-playlist` still expands a bare playlist URL into one JSON object
    // per entry; the queue models single videos only.
    ensure!(
        values.next().is_none(),
        "This URL points to a playlist. Paste the URL of a single video."
    );

    let duration = raw.duration.filter(|d| d.is_finite() && *d > 0.0);
    let mut formats: Vec<FormatInfo> = raw
        .formats
        .unwrap_or_default()
        .into_iter()
        .filter(|f| f.has_video() || f.has_audio())
        .collect();
    formats.iter_mut().for_each(|f| f.resolve_size(duration));

    Ok(Meta {
        title: raw
            .title
            .map(|t| t.trim().to_owned())
            .filter(|t| !t.is_empty())
            .unwrap_or_else(|| url.to_owned()),
        thumbnail: raw.thumbnail.filter(|t| !t.is_empty()),
        duration,
        chapters: raw.chapters.unwrap_or_default(),
        formats,
    })
}

// ── Size estimation ──────────────────────────────────────────────────────────

impl Meta {
    /// Best-effort total download size for `spec`. Every figure involving a
    /// preset, a clip or chapters is an estimate; only a plain format
    /// selection whose members all report an exact `filesize` is exact.
    pub fn estimate_size(&self, spec: &JobSpec) -> Option<SizeEstimate> {
        let whole = self.estimate_quality(&spec.quality)?;

        let seconds = match &spec.scope {
            Scope::Full => return Some(whole),
            Scope::Timeline { start, end } => {
                let start = if start.is_empty() { 0 } else { util::parse_clock(start)? };
                f64::from(util::parse_clock(end)?.checked_sub(start)?)
            }
            Scope::Chapters { selected } => selected
                .iter()
                .filter_map(|&i| self.chapters.get(i))
                .map(|c| (c.end - c.start).max(0.0))
                .sum(),
        };
        let ratio = (seconds / self.duration?).clamp(0.0, 1.0);
        Some(SizeEstimate { bytes: (whole.bytes as f64 * ratio) as u64, exact: false })
    }

    /// The audio-only stream a lone video-only `video` is completed with: the
    /// best one in the container yt-dlp can mux without switching the output
    /// to MKV, else the best audio stream overall. `None` when the video has
    /// no audio-only stream at all.
    pub fn audio_for(&self, video: &FormatInfo) -> Option<&FormatInfo> {
        let best = |accept: &dyn Fn(&FormatInfo) -> bool| {
            self.formats
                .iter()
                .filter(|f| f.kind() == StreamKind::AudioOnly && accept(f))
                .max_by(|a, b| cmp_keys(audio_key(a, false), audio_key(b, false)))
        };
        match matching_audio_ext(&video.ext) {
            Some(ext) => best(&|f| f.ext == ext).or_else(|| best(&|_| true)),
            None => best(&|_| true),
        }
    }

    /// The `-f` selector that downloads `ids`.
    ///
    /// * A lone video-only format is completed with audio, preferring the
    ///   stream that keeps the output in the video's own container, then any
    ///   audio, then the video alone when the site offers no audio.
    /// * Anything else (a combined format, a lone audio-only format, an
    ///   explicit video + audio pair kept from older rows) is passed as given.
    pub fn format_selector(&self, ids: &[String]) -> String {
        let [id] = ids else { return ids.join("+") };
        let video = self
            .formats
            .iter()
            .find(|f| f.format_id == *id && f.kind() == StreamKind::VideoOnly);
        match video {
            None => id.clone(),
            Some(video) => match matching_audio_ext(&video.ext) {
                Some(ext) => format!("{id}+bestaudio[ext={ext}]/{id}+bestaudio/{id}"),
                None => format!("{id}+bestaudio/{id}"),
            },
        }
    }

    /// The streams a full-video download of `quality` is expected to fetch,
    /// with the probe's size for each, in the order yt-dlp fetches them.
    /// Empty when the selection names a format the probe does not know, or
    /// matches nothing.
    ///
    /// This is the same pick `estimate_size` adds up, so the live total of a
    /// running download starts out equal to the estimate shown before it.
    /// Presets are approximations of yt-dlp's own choice: the queue pairs
    /// these streams with the real ones by `format_id`, then by position.
    pub fn expected_streams(&self, quality: &Quality) -> Vec<ExpectedStream> {
        let Some(pick) = self.pick(quality) else { return Vec::new() };
        pick.streams
            .into_iter()
            .map(|f| ExpectedStream {
                format_id: f.format_id.clone(),
                bytes: f.size_bytes,
                exact: pick.exact_possible && f.size_exact,
            })
            .collect()
    }

    fn estimate_quality(&self, quality: &Quality) -> Option<SizeEstimate> {
        let pick = self.pick(quality)?;

        let mut bytes = 0u64;
        let mut exact = pick.exact_possible;
        for f in pick.streams {
            // A partial sum would silently under-report, so one unknown
            // member makes the whole estimate unknown.
            bytes = bytes.saturating_add(f.size_bytes?);
            exact &= f.size_exact;
        }
        Some(SizeEstimate { bytes, exact })
    }

    /// The streams `quality` downloads. `None` when a named format is not in
    /// the probe, or when nothing matches.
    fn pick(&self, quality: &Quality) -> Option<Pick<'_>> {
        let (streams, exact_possible): (Vec<&FormatInfo>, bool) = match quality {
            Quality::Formats(ids) => {
                let mut picked: Vec<&FormatInfo> = ids
                    .iter()
                    .map(|id| self.formats.iter().find(|f| f.format_id == *id))
                    .collect::<Option<_>>()?;
                let lone_video = match picked.as_slice() {
                    [f] if f.kind() == StreamKind::VideoOnly => Some(*f),
                    _ => None,
                };
                // The audio is chosen by yt-dlp at download time, so adding it
                // makes the total an estimate even when every size is exact.
                let audio = lone_video.and_then(|v| self.audio_for(v));
                let exact = audio.is_none();
                picked.extend(audio);
                (picked, exact)
            }
            Quality::Preset(p) => (self.pick_for_preset(*p), false),
        };
        (!streams.is_empty()).then_some(Pick { streams, exact_possible })
    }

    /// Approximates the streams yt-dlp would pick for `preset`. The goal is a
    /// plausible size, not a faithful reimplementation of format sorting.
    fn pick_for_preset(&self, preset: Preset) -> Vec<&FormatInfo> {
        let all = || self.formats.iter();
        let audio_only = || all().filter(|f| f.kind() == crate::types::StreamKind::AudioOnly);
        let video_only = || all().filter(|f| f.kind() == crate::types::StreamKind::VideoOnly);
        let muxed = || all().filter(|f| f.kind() == crate::types::StreamKind::Muxed);

        match preset {
            Preset::Mp3 | Preset::M4a => audio_only()
                .max_by(|a, b| cmp_keys(audio_key(a, false), audio_key(b, false)))
                .or_else(|| muxed().max_by(|a, b| cmp_keys(video_key(a), video_key(b))))
                .into_iter()
                .collect(),

            Preset::Best => {
                let video = video_only().max_by(|a, b| cmp_keys(video_key(a), video_key(b)));
                let audio = audio_only().max_by(|a, b| cmp_keys(audio_key(a, false), audio_key(b, false)));
                match (video, audio) {
                    (Some(v), Some(a)) => vec![v, a],
                    _ => muxed()
                        .max_by(|a, b| cmp_keys(video_key(a), video_key(b)))
                        .into_iter()
                        .collect(),
                }
            }

            Preset::Video1080p => {
                let with_height: Vec<&FormatInfo> =
                    all().filter(|f| f.has_video() && f.height.is_some()).collect();
                let height = |f: &&FormatInfo| f.height.unwrap_or(0.0);
                let within: Vec<&FormatInfo> =
                    with_height.iter().copied().filter(|f| height(f) <= 1080.0).collect();
                let video = if within.is_empty() {
                    // Nothing at or below 1080p: the smallest step above it.
                    with_height.into_iter().min_by(|a, b| height(a).total_cmp(&height(b)))
                } else {
                    within.into_iter().max_by(|a, b| {
                        cmp_keys(
                            [height(a), f64::from(is_h264(a)), a.tbr.unwrap_or(0.0)],
                            [height(b), f64::from(is_h264(b)), b.tbr.unwrap_or(0.0)],
                        )
                    })
                };
                let Some(video) = video else { return Vec::new() };
                if video.has_audio() {
                    return vec![video];
                }
                let audio = audio_only().max_by(|a, b| cmp_keys(audio_key(a, true), audio_key(b, true)));
                std::iter::once(video).chain(audio).collect()
            }
        }
    }
}

/// The audio-only container that muxes into the same file type as a video of
/// container `video_ext` (MP4 + M4A stays MP4, WebM + WebM audio stays WebM).
/// `None` when no such pairing is known.
fn matching_audio_ext(video_ext: &str) -> Option<&'static str> {
    match video_ext {
        "mp4" | "m4v" | "mov" => Some("m4a"),
        "webm" => Some("webm"),
        _ => None,
    }
}

fn is_h264(f: &FormatInfo) -> bool {
    f.vcodec.starts_with("avc1") || f.vcodec.starts_with("h264")
}

fn is_aac(f: &FormatInfo) -> bool {
    f.acodec.starts_with("mp4a")
}

/// `[height, bitrate, 0]`, compared lexicographically.
fn video_key(f: &FormatInfo) -> [f64; 3] {
    [f.height.unwrap_or(0.0), f.tbr.unwrap_or(0.0), 0.0]
}

/// `[bitrate, 0, 0]`, or `[is_aac, bitrate, 0]` when AAC is preferred.
fn audio_key(f: &FormatInfo, prefer_aac: bool) -> [f64; 3] {
    let rate = f.abr.or(f.tbr).unwrap_or(0.0);
    if prefer_aac {
        [f64::from(is_aac(f)), rate, 0.0]
    } else {
        [rate, 0.0, 0.0]
    }
}

fn cmp_keys(a: [f64; 3], b: [f64; 3]) -> Ordering {
    a.iter()
        .zip(&b)
        .map(|(x, y)| x.total_cmp(y))
        .find(|o| o.is_ne())
        .unwrap_or(Ordering::Equal)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::test_support::fmt;

    fn sized(id: &str, v: &str, a: &str, height: Option<f64>, bytes: u64, exact: bool) -> FormatInfo {
        let mut f = fmt(id, v, a);
        f.height = height;
        f.size_bytes = Some(bytes);
        f.size_exact = exact;
        f
    }

    fn meta() -> Meta {
        let mut m = Meta {
            title: "t".into(),
            thumbnail: None,
            duration: Some(100.0),
            chapters: vec![
                Chapter { title: "a".into(), start: 0.0, end: 25.0 },
                Chapter { title: "b".into(), start: 25.0, end: 100.0 },
            ],
            formats: vec![
                sized("18", "avc1", "mp4a", Some(360.0), 10, true),
                sized("137", "avc1", "none", Some(1080.0), 1000, true),
                sized("248", "vp09", "none", Some(1080.0), 800, false),
                sized("401", "av01", "none", Some(2160.0), 4000, false),
                sized("140", "none", "mp4a", None, 100, true),
                sized("251", "none", "opus", None, 90, false),
            ],
        };
        m.formats[4].abr = Some(128.0);
        m.formats[5].abr = Some(100.0);
        for (i, ext) in ["mp4", "mp4", "webm", "mp4", "m4a", "webm"].into_iter().enumerate() {
            m.formats[i].ext = ext.into();
        }
        m
    }

    fn ids(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| (*s).to_owned()).collect()
    }

    fn spec(quality: Quality, scope: Scope) -> JobSpec {
        JobSpec { quality, scope }
    }

    #[test]
    fn explicit_formats_are_exact_only_when_every_member_is() {
        let m = meta();
        let e = m
            .estimate_size(&spec(Quality::Formats(vec!["137".into(), "140".into()]), Scope::Full))
            .unwrap();
        assert_eq!(e, SizeEstimate { bytes: 1100, exact: true });
        let e = m
            .estimate_size(&spec(Quality::Formats(vec!["248".into(), "140".into()]), Scope::Full))
            .unwrap();
        assert_eq!(e, SizeEstimate { bytes: 900, exact: false });
    }

    #[test]
    fn audio_follows_the_video_container() {
        let m = meta();
        let audio_id = |video: &str| {
            let v = m.formats.iter().find(|f| f.format_id == video).unwrap();
            m.audio_for(v).map(|a| a.format_id.clone())
        };
        assert_eq!(audio_id("137").as_deref(), Some("140"), "mp4 video takes m4a audio");
        assert_eq!(audio_id("248").as_deref(), Some("251"), "webm video takes webm audio");

        // A container with no known pairing, and a matching container that is
        // absent, both fall back to the best audio overall (140: 128 kbps).
        let mut mkv = m.formats[1].clone();
        mkv.ext = "mkv".into();
        assert_eq!(m.audio_for(&mkv).unwrap().format_id, "140");
        let mut no_m4a = meta();
        no_m4a.formats.retain(|f| f.format_id != "140");
        let mp4 = no_m4a.formats[1].clone();
        assert_eq!(no_m4a.audio_for(&mp4).unwrap().format_id, "251");

        let mut silent = meta();
        silent.formats.retain(|f| f.kind() != StreamKind::AudioOnly);
        assert!(silent.audio_for(&mp4).is_none());
    }

    #[test]
    fn selector_completes_a_lone_video_only_format() {
        let m = meta();
        assert_eq!(m.format_selector(&ids(&["137"])), "137+bestaudio[ext=m4a]/137+bestaudio/137");
        assert_eq!(m.format_selector(&ids(&["248"])), "248+bestaudio[ext=webm]/248+bestaudio/248");

        let mut mkv = meta();
        mkv.formats[1].ext = "mkv".into();
        assert_eq!(mkv.format_selector(&ids(&["137"])), "137+bestaudio/137");
    }

    #[test]
    fn selector_passes_everything_else_through() {
        let m = meta();
        assert_eq!(m.format_selector(&ids(&["18"])), "18", "combined format");
        assert_eq!(m.format_selector(&ids(&["140"])), "140", "lone audio-only format");
        assert_eq!(m.format_selector(&ids(&["137", "140"])), "137+140", "explicit pair");
        assert_eq!(m.format_selector(&ids(&["999"])), "999", "format missing from the probe");
    }

    #[test]
    fn lone_video_only_estimate_includes_its_audio() {
        let m = meta();
        let e = m.estimate_size(&spec(Quality::Formats(ids(&["137"])), Scope::Full)).unwrap();
        assert_eq!(e, SizeEstimate { bytes: 1000 + 100, exact: false });
        let e = m.estimate_size(&spec(Quality::Formats(ids(&["248"])), Scope::Full)).unwrap();
        assert_eq!(e, SizeEstimate { bytes: 800 + 90, exact: false });

        // No audio on offer: the video alone, and exact if its own size is.
        let mut silent = meta();
        silent.formats.retain(|f| f.kind() != StreamKind::AudioOnly);
        let e = silent.estimate_size(&spec(Quality::Formats(ids(&["137"])), Scope::Full)).unwrap();
        assert_eq!(e, SizeEstimate { bytes: 1000, exact: true });

        // The audio it would add has no known size: so the total is unknown.
        let mut unsized_audio = meta();
        unsized_audio.formats[4].size_bytes = None;
        assert!(unsized_audio.estimate_size(&spec(Quality::Formats(ids(&["137"])), Scope::Full)).is_none());
    }

    #[test]
    fn presets_pick_plausible_streams_and_are_never_exact() {
        let m = meta();
        let best = m.estimate_size(&spec(Quality::Preset(Preset::Best), Scope::Full)).unwrap();
        assert_eq!(best, SizeEstimate { bytes: 4000 + 100, exact: false });
        let hd = m.estimate_size(&spec(Quality::Preset(Preset::Video1080p), Scope::Full)).unwrap();
        // 1080p h264 video-only + AAC audio.
        assert_eq!(hd.bytes, 1000 + 100);
        let mp3 = m.estimate_size(&spec(Quality::Preset(Preset::Mp3), Scope::Full)).unwrap();
        assert!(!mp3.exact);
    }

    #[test]
    fn clips_and_chapters_scale_by_duration() {
        let m = meta();
        let q = Quality::Formats(vec!["18".into()]);
        let clip = Scope::Timeline { start: "00:00:00".into(), end: "00:00:50".into() };
        assert_eq!(m.estimate_size(&spec(q.clone(), clip)).unwrap(), SizeEstimate { bytes: 5, exact: false });
        let ch = Scope::Chapters { selected: vec![0] };
        assert_eq!(m.estimate_size(&spec(q, ch)).unwrap().bytes, 2);
    }

    #[test]
    fn unknown_member_size_makes_the_estimate_unknown() {
        let mut m = meta();
        m.formats[4].size_bytes = None; // 140, the only AAC audio
        assert!(m.estimate_size(&spec(Quality::Formats(vec!["137".into(), "140".into()]), Scope::Full)).is_none());
    }

    #[test]
    fn expected_streams_are_the_streams_the_estimate_adds_up() {
        let m = meta();
        let stream = |id: &str, bytes: Option<u64>, exact: bool| ExpectedStream {
            format_id: id.into(),
            bytes,
            exact,
        };
        let expected = |list: &[&str]| m.expected_streams(&Quality::Formats(ids(list)));

        // An explicit pair keeps its order, and exact sizes stay exact.
        assert_eq!(
            expected(&["137", "140"]),
            [stream("137", Some(1000), true), stream("140", Some(100), true)]
        );
        assert_eq!(
            expected(&["251", "137"]),
            [stream("251", Some(90), false), stream("137", Some(1000), true)]
        );
        // A combined format is a single stream.
        assert_eq!(expected(&["18"]), [stream("18", Some(10), true)]);
        // A lone video-only format is completed with audio yt-dlp picks at
        // download time, so neither size can be called exact.
        assert_eq!(
            expected(&["137"]),
            [stream("137", Some(1000), false), stream("140", Some(100), false)]
        );
        // A preset is yt-dlp's choice, approximated; never exact.
        assert_eq!(
            m.expected_streams(&Quality::Preset(Preset::Best)),
            [stream("401", Some(4000), false), stream("140", Some(100), false)]
        );

        // The sizes are exactly what `estimate_size` adds up.
        let total: u64 = expected(&["137"]).iter().filter_map(|s| s.bytes).sum();
        let estimate = m.estimate_size(&spec(Quality::Formats(ids(&["137"])), Scope::Full)).unwrap();
        assert_eq!(total, estimate.bytes);
    }

    #[test]
    fn expected_streams_keep_an_unknown_size_and_give_up_on_unknown_formats() {
        let mut m = meta();
        m.formats[4].size_bytes = None; // 140
        let got = m.expected_streams(&Quality::Formats(ids(&["137", "140"])));
        assert_eq!(got.iter().map(|s| s.bytes).collect::<Vec<_>>(), [Some(1000), None]);
        // The estimate is unknown then, but the streams are still known.
        assert!(m.estimate_size(&spec(Quality::Formats(ids(&["137", "140"])), Scope::Full)).is_none());

        assert!(m.expected_streams(&Quality::Formats(ids(&["999"]))).is_empty());
        assert!(m.expected_streams(&Quality::Formats(ids(&["137", "999"]))).is_empty());
        m.formats.clear();
        assert!(m.expected_streams(&Quality::Preset(Preset::Best)).is_empty());
    }

    #[test]
    fn parse_handles_sparse_output_and_filters_empty_streams() {
        let json = r#"{"formats":[{"format_id":"a","vcodec":"none","acodec":"none"},{"format_id":"b","filesize":5}]}"#;
        let meta = parse(json.as_bytes(), "http://x/y").unwrap();
        assert_eq!(meta.title, "http://x/y");
        assert_eq!(meta.formats.len(), 1);
        assert_eq!(meta.formats[0].size_bytes, Some(5));
        assert!(meta.chapters.is_empty());
        assert_eq!(meta.duration, None);
    }

    #[test]
    fn parse_rejects_playlists_and_empty_output() {
        let two = "{\"title\":\"a\"}\n{\"title\":\"b\"}\n";
        assert!(parse(two.as_bytes(), "u").unwrap_err().to_string().contains("playlist"));
        assert!(parse(b"", "u").is_err());
        assert!(parse(b"not json", "u").is_err());
    }
}
