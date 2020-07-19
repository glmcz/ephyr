//! [`nginx-vod-module` mapping][1] response format.
//!
//! [1]: https://github.com/kaltura/nginx-vod-module#mapping-response-format

use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    time::Duration,
};

use chrono::{
    serde::ts_milliseconds, DateTime, Datelike, Duration as DateDuration, Utc,
};
use derive_more::{From, Into};
use isolang::Language;
use serde::{Deserialize, Serialize};
use smart_default::SmartDefault;
use url::Url;

use crate::vod::state;

/// Top level object in the [`nginx-vod-module` mapping][2] JSON, representing
/// several [`Sequence`]s that play together as an [adaptive set][1].
///
/// [1]: https://tinyurl.com/ng-vod#set-top-level-object-in-the-mapping-json
/// [2]: https://github.com/kaltura/nginx-vod-module#mapping-response-format
#[derive(Clone, Debug, Deserialize, Serialize, SmartDefault)]
#[serde(rename_all = "camelCase")]
pub struct Set {
    /// String that identifies this [`Set`]. It can be retrieved by
    /// `$vod_set_id`.
    ///
    /// By default, [`state::Playlist::slug`] is used.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,

    /// Playlist type of this [`Set`].
    #[serde(default)]
    pub playlist_type: PlaylistType,

    /// Indicator whether the different [`Clip`]s in each [`Sequence`] have
    /// different media parameters.
    ///
    /// This field has different manifestations according to the delivery
    /// protocol - a value of `true` will generate `#EXT-X-DISCONTINUITY` in
    /// [HLS], and a multi period MPD in [DASH][1].
    ///
    /// The default value is `true`, set to `false` only if the media files were
    /// transcoded with exactly the same parameters.
    ///
    /// [HLS]: https://en.wikipedia.org/wiki/HTTP_Live_Streaming
    /// [1]: https://en.wikipedia.org/wiki/Dynamic_Adaptive_Streaming_over_HTTP
    #[default = true]
    #[serde(default)]
    pub discontinuity: bool,

    /// [`Clip`] durations in milliseconds. It must contain at least one element
    /// and up to [`Clip::MAX_DURATIONS_LEN`] elements.
    #[serde(default)]
    pub durations: Vec<MillisDuration>,

    /// Absolute times of all the [`Clip`]s in the playlist, in milliseconds
    /// [since the epoch][1]. This field can be used only when
    /// [`Set::discontinuity`] is set to `true`. The timestamps may contain
    /// gaps, but they are not allowed to overlap:
    /// `set.clip_times[n + 1] >= set.clip_times[n] + set.durations[n]`.
    ///
    /// [1]: https://en.wikipedia.org/wiki/Unix_time
    #[serde(default)]
    pub clip_times: Vec<MillisDateTime>,

    /// Adaptive set of [`Sequence`]s of this mapping. The mapping has to
    /// contain at least one sequence and up to 32 sequences.
    ///
    /// Each [`Sequence`] must have the same number of [`Clip`]s.
    pub sequences: Vec<Sequence>,
}

impl Set {
    /// Maximum length that [`Set::durations`] can hold.
    pub const MAX_DURATIONS_LEN: usize = 128;
}

impl From<&state::Playlist> for Set {
    fn from(pl: &state::Playlist) -> Self {
        let mut set = Self {
            id: Some(pl.slug.clone()),
            playlist_type: PlaylistType::Live,
            discontinuity: true,
            ..Self::default()
        };

        // Because all `Set::sequences` must have the same length, we should
        // define the minimal mutual intersection of all quality sizes and use
        // only them to form a `Set`.
        let sizes = pl.mutual_src_sizes();
        if sizes.is_empty() {
            return set;
        }
        let mut sequences: HashMap<_, _> = sizes
            .iter()
            .map(|size| {
                let sequence = Sequence {
                    id: Some(format!("{}p", *size as u16)),
                    language: Some(pl.lang),
                    label: Some(format!("{}p", *size as u16)),
                    ..Sequence::default()
                };
                (*size, sequence)
            })
            .collect();

        let now = Utc::now().with_timezone(&pl.tz);
        let mut today = now.date().and_hms(0, 0, 0);
        'whole_loop: for i in 0..7 {
            let in_today = i == 0;
            let tomorrow = today + DateDuration::days(1);

            if let Some(today_clips) = pl.clips.get(&today.weekday()) {
                let mut time = today;

                // Unfortunately, nginx-vod-module loops the whole playlist
                // only, and is unable to loop a part of playlist in the given
                // time window. That's why, to loop all clips of today's day
                // without affecting tomorrow's playlist, we need to repeat the
                // playlist manually, until tomorrow comes.
                'day_loop: while time < tomorrow {
                    let mut is_at_least_one_clip_considered = false;

                    for clip in today_clips {
                        let clip_duration = clip.view.to - clip.view.from;
                        let next_time = time
                            + DateDuration::from_std(clip_duration).unwrap();

                        // There is no sense to return today's clips, which have
                        // been already finished. Instead, we start from the
                        // first non-finished today's clip. This way we reserve
                        // more space for future clips, considering the
                        // nginx-vod-module `Set::MAX_DURATIONS_LEN` limitation.
                        let should_skip = in_today && next_time <= now;

                        // "Considered" means that clip's duration is considered
                        // for building the sequence timestamps. However, it
                        // doesn't necessarily mean that clip is added to this
                        // sequence.
                        let mut is_clip_considered = false;

                        for (size, src) in &clip.sources {
                            if let Some(seq) = sequences.get_mut(size) {
                                if !should_skip {
                                    seq.clips.push(Clip {
                                        r#type: SourceClip {
                                            path: SourceClip::parse_url_path(
                                                src.url
                                                    .local
                                                    .as_ref()
                                                    .unwrap_or(
                                                        &src.url.upstream,
                                                    ),
                                            ),
                                            from: Some(clip.view.from.into()),
                                            to: Some(clip.view.to.into()),
                                        }
                                        .into(),
                                    });
                                }

                                is_clip_considered = true;
                            }
                        }

                        if !is_clip_considered {
                            continue;
                        }
                        is_at_least_one_clip_considered = true;

                        if !should_skip {
                            set.clip_times
                                .push(time.clone().with_timezone(&Utc).into());

                            set.durations.push(clip_duration.into());
                            if set.durations.len() >= Self::MAX_DURATIONS_LEN {
                                break 'whole_loop;
                            }
                        }

                        time = next_time;
                        if time >= tomorrow {
                            break 'day_loop;
                        }
                    }

                    if !is_at_least_one_clip_considered {
                        break;
                    }
                }
            }

            today = tomorrow;
        }

        set.sequences = sequences.into_iter().map(|(_, seq)| seq).collect();
        set
    }
}

/// Possible playlist types of [`Set`].
#[derive(
    Clone, Copy, Debug, Deserialize, Eq, Serialize, SmartDefault, PartialEq,
)]
#[serde(rename_all = "lowercase")]
pub enum PlaylistType {
    /// Live stream type.
    Live,

    /// [VOD] (video on demand) type.
    ///
    /// [VOD]: https://en.wikipedia.org/wiki/Video_on_demand
    #[default]
    Vod,
}

/// [Sequence][1] of [`Clip`]s that should be played one after the other.
///
/// [1]: https://github.com/kaltura/nginx-vod-module#sequence
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct Sequence {
    /// String that identifies this [`Sequence`]. It can be retrieved by
    /// `$vod_sequence_id`.
    ///
    /// By default is named after [`state::SrcSize`] this [`Sequence`] holds
    /// videos of.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,

    /// Language of this [`Sequence`].
    ///
    /// This field takes priority over any language specified on the media file.
    ///
    /// By default uses [`state::Playlist::lang`] value.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub language: Option<Language>,

    /// Friendly string that identifies this [`Sequence`]. If a
    /// [`Sequence::language`] is specified, a default label will be
    /// automatically derived by it (e.g. if language is `ita`, by default
    /// `italiano` will be used as the label).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,

    /// [`Clip`]s which form this [`Sequence`].
    ///
    /// The number of elements must match the number of [`Set::durations`].
    pub clips: Vec<Clip>,
}

/// [Clip][1] to be played in a [`Sequence`]. Represents the result of applying
/// zero or more filters on a set of [`SourceClip`]s.
///
/// [1]: https://github.com/kaltura/nginx-vod-module#clip-abstract
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct Clip {
    /// Type of this [`Clip`].
    #[serde(flatten)]
    pub r#type: ClipType,
}

/// Supported [clip types][1].
///
/// [1]: https://github.com/kaltura/nginx-vod-module#clip-abstract
#[derive(Clone, Debug, Deserialize, From, Serialize)]
#[serde(tag = "type", rename_all = "camelCase")]
pub enum ClipType {
    /// [Source clip][1] type.
    ///
    /// [1]: https://github.com/kaltura/nginx-vod-module#source-clip
    Source(SourceClip),
}

/// [Source clip][1] representing a [MP4] file to be played.
///
/// [MP4]: https://en.wikipedia.org/wiki/MPEG-4_Part_14
/// [1]: https://github.com/kaltura/nginx-vod-module#source-clip
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct SourceClip {
    /// Path of the MP4 file, where it should be read from.
    ///
    /// The string `"empty"` can be used to represent an empty captions file
    /// (useful in case only some videos in a playlist have captions).
    ///
    /// If [`vod_remote_upstream_location` directive][1] is specified in Nginx
    /// configuration, the this path is treated as URL path in that location.
    /// Otherwise, this path is treated as a filesystem path.
    ///
    /// [1]: https://tinyurl.com/ng-vod#vod_remote_upstream_location
    pub path: PathBuf,

    /// Offset in milliseconds, from the beginning of the media file, from which
    /// to start loading frames (inclusive).
    ///
    /// If not specified, then loading frames starts from the very beginning of
    /// the media file.
    #[serde(rename = "clipFrom")]
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub from: Option<MillisDuration>,

    /// Offset in milliseconds, from the beginning of the media file, until
    /// which to load frames (exclusive).
    ///
    /// Obviously, should be always greater than [`SourceClip::from`].
    ///
    /// If not specified, then loading frames is done until the end of the media
    /// file is reached.
    #[serde(rename = "clipTo")]
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub to: Option<MillisDuration>,
}

impl SourceClip {
    /// Transforms the given source file URL into a [`SourceClip::path`]
    /// acceptable by the [`nginx-vod-module`][1].
    ///
    /// [1]: https://github.com/kaltura/nginx-vod-module
    #[must_use]
    pub fn parse_url_path(url: &Url) -> PathBuf {
        let (old_prefix, new_prefix) = match url.scheme() {
            "file" => ("/var/lib/ephyr/vod", "/local"),
            "http" | "https" => match url.host() {
                Some(url::Host::Domain("api.allatra.video")) => {
                    ("/storage/videos", "/api.allatra.video")
                }
                _ => panic!(
                    "Unsupported remote source URL host for nginx-vod-module: \
                     {}",
                    url,
                ),
            },
            _ => panic!(
                "Unsupported source URL schema for nginx-vod-module: {}",
                url,
            ),
        };
        PathBuf::from(new_prefix)
            .join(Path::new(url.path()).strip_prefix(old_prefix).unwrap())
    }
}

/// [`Duration`] which serializes/deserializes into/from whole milliseconds.
///
/// [1]: https://en.wikipedia.org/wiki/Unix_time
#[derive(
    Clone,
    Copy,
    Debug,
    Default,
    Deserialize,
    Eq,
    From,
    Into,
    PartialEq,
    Serialize,
)]
pub struct MillisDuration(#[serde(with = "serde_millis")] Duration);

/// [`DateTime`] which serializes/deserializes into/from
/// [UNIX epoch timestamp][1] in milliseconds.
///
/// [1]: https://en.wikipedia.org/wiki/Unix_time
#[derive(Clone, Debug, Deserialize, Eq, From, Into, PartialEq, Serialize)]
pub struct MillisDateTime(#[serde(with = "ts_milliseconds")] DateTime<Utc>);

#[cfg(test)]
mod spec {
    use super::*;

    #[test]
    fn serializes() {
        let mapping = Set {
            id: Some("hi".into()),
            durations: vec![
                Duration::from_secs(83).into(),
                Duration::from_secs(83).into(),
            ],
            clip_times: vec![
                Utc::now().into(),
                (Utc::now() + chrono::Duration::seconds(83)).into(),
            ],
            sequences: vec![Sequence {
                clips: vec![
                    Clip {
                        r#type: SourceClip {
                            path: "/path/to/video1.mp4".into(),
                            ..SourceClip::default()
                        }
                        .into(),
                    },
                    Clip {
                        r#type: SourceClip {
                            path: "/path/to/video2.mp4".into(),
                            ..SourceClip::default()
                        }
                        .into(),
                    },
                ],
                ..Sequence::default()
            }],
            ..Set::default()
        };

        let res = serde_json::to_string_pretty(&mapping);

        assert!(res.is_ok(), "serialization fails");
    }
}
