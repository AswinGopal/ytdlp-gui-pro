use crate::types::Preset;

/// The yt-dlp arguments a `Preset` adds on top of the base invocation.
///
/// Presets never select a format ID themselves; they steer yt-dlp through
/// post-processing (`-x`) or format sorting (`-S`), so they compose with any
/// download scope (full video, timeline clip, chapters).
pub fn extra_args(p: Preset) -> Vec<String> {
    let args: &[&str] = match p {
        Preset::Mp3 => &["-x", "--audio-format", "mp3"],
        Preset::M4a => &["-x", "--audio-format", "m4a"],
        Preset::Video1080p => &[
            "-S",
            "res:1080,vcodec:h264,acodec:aac",
            "--merge-output-format",
            "mp4",
        ],
        Preset::Best => &[],
    };
    args.iter().map(|&a| a.to_owned()).collect()
}
