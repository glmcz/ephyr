//! Definitions of [allatra.video][1] site API.
//!
//! [1]: https://allatra.video

use anyhow::anyhow;
use mime::Mime;
use serde::{Deserialize, Serialize};
use serde_repr::{Deserialize_repr, Serialize_repr};
use std::time::Duration;
use url::Url;

use crate::util::serde::timelike;

/// [API] of [allatra.video][1] site.
///
/// [API]: https://en.wikipedia.org/wiki/Application_programming_interface
/// [1]: https://allatra.video
#[derive(Clone, Copy, Debug)]
pub struct Api;

impl Api {
    /// [URL] of the [allatra.video][1] site API v1.
    ///
    /// [URL]: https://en.wikipedia.org/wiki/URL
    /// [1]: https://allatra.video
    pub const V1_URL: &'static str = "https://api.allatra.video/api/v1";

    /// Performs `GET /videos/yt/{youTubeHash}` API request, returning the
    /// parsed [`Video`], if any.
    ///
    /// # Errors
    ///
    /// If API request cannot be performed, or fails.
    #[allow(clippy::ptr_arg)]
    pub async fn get_videos_yt(id: &YoutubeId) -> Result<Video, anyhow::Error> {
        let resp = reqwest::get(&format!("{}/videos/yt/{}", Api::V1_URL, id))
            .await
            .map_err(|e| anyhow!("Failed to perform API request: {}", e))?;
        if !resp.status().is_success() {
            return Err(anyhow!(
                "API request failed with status {}",
                resp.status(),
            ));
        }
        Ok(resp
            .json::<Response<Video>>()
            .await
            .map_err(|e| anyhow!("Failed to decode API response: {}", e))?
            .data)
    }
}

/// Successful response, returned by [allatra.video][1] site API.
///
/// [1]: https://allatra.video
#[derive(Clone, Copy, Debug, Deserialize, Serialize)]
pub struct Response<T> {
    /// Data returned by this [`Response`].
    pub data: T,
}

/// Video model returned by [allatra.video][1] site API.
///
/// # Notice
///
/// At the moment, it doesn't describes the whole model. Only the part required
/// for the needs of this application.
///
/// [1]: https://allatra.video
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Video {
    /// ID of this [`Video`] on [YouTube].
    ///
    /// [YouTube]: https://youtube.com
    pub youtube_id: YoutubeId,

    /// Total duration of this [`Video`].
    #[serde(rename = "durationFormatted", with = "timelike")]
    pub duration: Duration,

    /// [`Source`]s of this [`Video`] where it can be read from.
    pub sources: Vec<Source>,
}

// TODO: Make as an optimized newtype:
//       https://webapps.stackexchange.com/a/101153
/// ID of a [`Video`] on [YouTube].
///
/// [YouTube]: https://youtube.com
pub type YoutubeId = String;

/// Source file of a [`Video`].
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct Source {
    /// [URL] of this [`Source`] file, where it can be read from.
    ///
    /// [URL]: https://en.wikipedia.org/wiki/URL
    pub src: Url,

    /// [MIME type][1] of this [`Source`] file.
    ///
    /// [1]: https://en.wikipedia.org/wiki/Media_type
    #[serde(with = "mime_serde_shim")]
    pub r#type: Mime,

    /// Resolution of the [`Video`] contained in this [`Source`] file.
    pub size: Resolution,
}

/// Supported [video resolutions][1] of a [`Video`].
///
/// These are basically the ones [supported by YouTube][2] (not all, though).
///
/// [1]: https://en.wikipedia.org/wiki/Display_resolution
/// [2]: https://support.google.com/youtube/answer/6375112
#[derive(
    Clone, Copy, Debug, Deserialize_repr, Eq, Hash, PartialEq, Serialize_repr,
)]
#[repr(u16)]
pub enum Resolution {
    /// 240p [LDTV] (low-definition television) resolution.
    ///
    /// [LDTV]: https://en.wikipedia.org/wiki/Low-definition_television
    P240 = 240,

    /// 360p [LDTV] (low-definition television) resolution.
    ///
    /// [LDTV]: https://en.wikipedia.org/wiki/Low-definition_television
    P360 = 360,

    /// [480p] [EDTV] (enhanced-definition television) resolution.
    ///
    /// [480p]: https://en.wikipedia.org/wiki/480p
    /// [EDTV]: https://en.wikipedia.org/wiki/Enhanced-definition_television
    P480 = 480,

    /// [720p] (standard HD) [HDTV] (high-definition television) resolution.
    ///
    /// [720p]: https://en.wikipedia.org/wiki/720p
    /// [HDTV]: https://en.wikipedia.org/wiki/High-definition_television
    P720 = 720,

    /// [1080p] (full HD) [HDTV] (high-definition television) resolution.
    ///
    /// [1080p]: https://en.wikipedia.org/wiki/1080p
    /// [HDTV]: https://en.wikipedia.org/wiki/High-definition_television
    P1080 = 1080,
}

#[cfg(test)]
mod spec {
    use super::*;

    #[tokio::test]
    async fn retrieves_truth_of_life() {
        let res = Api::get_videos_yt(&"Q69gFVmrCiI".into()).await;
        assert!(
            res.is_ok(),
            "failed to request 'Q69gFVmrCiI' video in API: {}",
            res.unwrap_err(),
        );

        let video = res.unwrap();
        assert_eq!(
            video.youtube_id, "Q69gFVmrCiI",
            "YouTube ID parsed incorrectly",
        );
        assert_eq!(
            video.duration,
            Duration::from_secs(8639),
            "duration parsed incorrectly",
        );
        assert_eq!(video.sources.len(), 5, "sources set parsed incorrectly");
        for (i, source) in video.sources.iter().enumerate() {
            assert_eq!(
                source.src.as_ref(),
                &format!(
                    "https://api.allatra.video/storage/videos/mj/W7/5939\
                                                      /Q69gFVmrCiI_{}p.mp4",
                    source.size as u16,
                ),
                "URL parsed incorrectly for source {}",
                i,
            );
            assert_eq!(
                source.r#type, "video/mp4",
                "MIME type parsed incorrectly for source {}",
                i,
            );
        }
    }

    #[tokio::test]
    async fn parses_life_circle() {
        let resp =
            reqwest::get(&format!("{}/videos/yt/0wAtNWA93hM", Api::V1_URL))
                .await
                .expect("Failed to perform API request");
        if !resp.status().is_success() {
            panic!("Cannot access '0wAtNWA93hM' video in API");
        }

        let res = resp.json::<Response<Video>>().await;
        assert!(res.is_ok(), "cannot deserialize: {}", res.unwrap_err());

        let video = res.unwrap().data;
        assert_eq!(
            video.youtube_id, "0wAtNWA93hM",
            "YouTube ID parsed incorrectly",
        );
        assert_eq!(
            video.duration,
            Duration::from_secs(6686),
            "duration parsed incorrectly",
        );
        assert_eq!(video.sources.len(), 5, "sources set parsed incorrectly");
        for (i, source) in video.sources.iter().enumerate() {
            assert_eq!(
                source.src.as_ref(),
                &format!(
                    "https://api.allatra.video/storage/videos/0A/w4/8679\
                                                      /0wAtNWA93hM_{}p.mp4",
                    source.size as u16,
                ),
                "URL parsed incorrectly for source {}",
                i,
            );
            assert_eq!(
                source.r#type, "video/mp4",
                "MIME type parsed incorrectly for source {}",
                i,
            );
        }
    }
}
