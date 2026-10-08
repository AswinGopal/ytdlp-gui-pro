use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::types::deser_string;
use crate::util;

/// A chapter as reported by yt-dlp's info dict (`start_time` / `end_time`).
///
/// `alias` only widens what is *accepted* on deserialization, so the same
/// struct round-trips through the queue database and serialises to the
/// frontend with the short `start` / `end` names.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Chapter {
    #[serde(default, deserialize_with = "deser_string")]
    pub title: String,
    #[serde(alias = "start_time")]
    pub start: f64,
    #[serde(alias = "end_time")]
    pub end: f64,
}

/// One chapter ready to be handed to `yt::run`.
#[derive(Clone, Debug)]
pub struct ChapterPlan {
    /// Zero-based position in the video's full chapter list.
    pub index: usize,
    pub title: String,
    /// `--download-sections` specifier, `*HH:MM:SS-HH:MM:SS`.
    pub section: String,
    /// Output file stem as yt-dlp's `-o` template takes it (`%` escaped as
    /// `%%`), e.g. `03 - Intro`, zero-padded to the width of the full chapter
    /// count so the files sort in video order.
    pub output_name: String,
    /// The same stem as it appears in the file name on disk (no escaping).
    pub file_stem: String,
}

/// Builds the ordered download plan for the `selected` chapter indices.
///
/// Indices are de-duplicated, sorted ascending and out-of-range entries are
/// dropped, so the plan order is stable. The queue relies on that stability:
/// it persists only *how many* planned chapters have finished.
pub fn plan(all: &[Chapter], selected: &[usize]) -> Vec<ChapterPlan> {
    let width = util::digit_width(all.len()).max(2);

    let mut indices: Vec<usize> = selected.iter().copied().filter(|&i| i < all.len()).collect();
    indices.sort_unstable();
    indices.dedup();

    indices
        .into_iter()
        .map(|index| {
            let chapter = &all[index];
            let output_name = format!(
                "{:0width$} - {}",
                index + 1,
                util::sanitize_filename(&chapter.title),
            );
            ChapterPlan {
                index,
                title: chapter.title.clone(),
                section: format!(
                    "*{}-{}",
                    util::seconds_to_hms(chapter.start),
                    util::seconds_to_hms(chapter.end)
                ),
                // `sanitize_filename` turns every `%` into `%%` and produces
                // `%%` nowhere else, so undoing it is exact.
                file_stem: output_name.replace("%%", "%"),
                output_name,
            }
        })
        .collect()
}

/// The file path of every planned chapter, derived from the path yt-dlp
/// reported for the first one (`sample`).
///
/// A chapter's file is its stem plus a tail (resolution tag and extension)
/// that is the same for all of them, because they share one format selection.
/// One yt-dlp call therefore names the whole job. `None` when `sample` does
/// not start with the first stem — yt-dlp shortened the name, say — so the
/// caller skips its check instead of guessing.
pub fn sibling_paths(plans: &[ChapterPlan], sample: &Path) -> Option<Vec<PathBuf>> {
    let first = plans.first()?;
    let dir = sample.parent()?;
    let tail = sample.file_name()?.to_str()?.strip_prefix(first.file_stem.as_str())?;
    Some(plans.iter().map(|p| dir.join(format!("{}{tail}", p.file_stem))).collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn chapters(n: usize) -> Vec<Chapter> {
        (0..n)
            .map(|i| Chapter {
                title: format!("Part {i}"),
                start: i as f64 * 60.0,
                end: (i + 1) as f64 * 60.0,
            })
            .collect()
    }

    #[test]
    fn plan_sorts_dedups_and_drops_out_of_range() {
        let p = plan(&chapters(3), &[2, 0, 2, 9]);
        let idx: Vec<usize> = p.iter().map(|c| c.index).collect();
        assert_eq!(idx, [0, 2]);
        assert_eq!(p[1].section, "*00:02:00-00:03:00");
        assert_eq!(p[1].output_name, "03 - Part 2");
    }

    #[test]
    fn plan_pads_to_total_chapter_count() {
        let p = plan(&chapters(120), &[4]);
        assert_eq!(p[0].output_name, "005 - Part 4");
    }

    #[test]
    fn file_stem_is_the_unescaped_output_name() {
        let mut all = chapters(2);
        all[0].title = "100% sure: a/b".into();
        all[1].title = "50%% off".into();
        let p = plan(&all, &[0, 1]);
        assert_eq!(p[0].output_name, "01 - 100%% sure_ a_b");
        assert_eq!(p[0].file_stem, "01 - 100% sure_ a_b");
        assert_eq!(p[1].output_name, "02 - 50%%%% off");
        assert_eq!(p[1].file_stem, "02 - 50%% off");
    }

    #[test]
    fn sibling_paths_share_the_tail_of_the_sample() {
        let p = plan(&chapters(3), &[0, 2]);
        let sample = Path::new("/d/01 - Part 0 [1080p].mkv");
        let paths = sibling_paths(&p, sample).unwrap();
        assert_eq!(
            paths,
            [PathBuf::from("/d/01 - Part 0 [1080p].mkv"), PathBuf::from("/d/03 - Part 2 [1080p].mkv")]
        );

        // Audio chapters have no tag; the tail is just the extension.
        let audio = sibling_paths(&p, Path::new("/d/01 - Part 0.mp3")).unwrap();
        assert_eq!(audio[1], PathBuf::from("/d/03 - Part 2.mp3"));
    }

    #[test]
    fn sibling_paths_give_up_when_the_name_does_not_match() {
        let p = plan(&chapters(2), &[0, 1]);
        assert!(sibling_paths(&p, Path::new("/d/01 - Pa [1080p].mp4")).is_none());
        assert!(sibling_paths(&[], Path::new("/d/x.mp4")).is_none());
    }

    #[test]
    fn deserializes_yt_dlp_and_own_shape() {
        let raw: Chapter =
            serde_json::from_str(r#"{"title":"A","start_time":1.5,"end_time":9}"#).unwrap();
        assert_eq!((raw.start, raw.end), (1.5, 9.0));
        let json = serde_json::to_string(&raw).unwrap();
        let back: Chapter = serde_json::from_str(&json).unwrap();
        assert_eq!((back.start, back.end), (1.5, 9.0));
        let null_title: Chapter =
            serde_json::from_str(r#"{"title":null,"start_time":0,"end_time":1}"#).unwrap();
        assert!(null_title.title.is_empty());
    }
}
