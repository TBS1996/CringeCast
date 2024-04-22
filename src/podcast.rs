use crate::config::DownloadMode;
use crate::config::EvalData;
use crate::config::PodcastConfig;
use crate::config::PodcastConfigs;
use crate::config::{Config, GlobalConfig};
use crate::display::DownloadBar;
use crate::episode::DownloadedEpisode;
use crate::episode::Episode;
use crate::episode::RawEpisode;
use crate::tags;
use crate::utils;
use futures::future;
use indicatif::MultiProgress;
use quickxml_to_serde::{xml_string_to_json, Config as XmlConfig};
use serde_json::Map;
use serde_json::Value;
use std::path::PathBuf;
use std::sync::Arc;

/// Converts the podcast's xml string to serde values of the channel and the episodes.
///
/// The library will merge different namespaces together, which is why we manually change
/// the itunes namespace, and then after converting it, we change it back. Preserving itunes:XXX as
/// separate keys.
fn xml_to_value(xml: &str) -> Option<(RawPodcast, Vec<RawEpisode>)> {
    let placeholder = "__placeholder__";
    let replacement = format!("itunes{}", placeholder);
    let xml = xml.replace("itunes:", &replacement);
    let conf = XmlConfig::new_with_defaults();
    let mut val = std::mem::take(
        xml_string_to_json(xml.to_string(), &conf)
            .ok()?
            .get_mut("rss")?
            .get_mut("channel")?,
    );

    // Create a new map to store the transformed keys at the top level
    let mut new_map: Map<String, Value> = Map::new();

    if let Some(obj) = val.as_object() {
        for (key, value) in obj {
            let new_key = key.replace(&replacement, "itunes:");
            new_map.insert(new_key, value.clone());
        }
    }

    let podcast = RawPodcast::new(new_map);

    let items = std::mem::take(val.as_object_mut()?.get_mut("item")?.as_array_mut()?);

    let episodes = items
        .iter()
        .map(|item| {
            let mut new_item_map: Map<String, Value> = Map::new();
            for (key, val) in item.as_object().unwrap().iter() {
                let new_key = key.replace(&replacement, "itunes:");
                new_item_map.insert(new_key, val.clone());
            }
            RawEpisode::new(new_item_map)
        })
        .collect::<Vec<RawEpisode>>();

    Some((podcast, episodes))
}

pub struct Podcasts {
    mp: MultiProgress,
    configs: PodcastConfigs,
    client: Arc<reqwest::Client>,
    global_config: Arc<GlobalConfig>,
}

impl Podcasts {
    pub fn new(global_config: GlobalConfig, configs: PodcastConfigs) -> Self {
        let mp = MultiProgress::new();
        let global_config = Arc::new(global_config);

        let client = reqwest::Client::builder()
            .user_agent(&global_config.user_agent())
            .build()
            .map(Arc::new)
            .expect("error: failed to instantiate reqwest client");

        Self {
            mp,
            client,
            configs,
            global_config,
        }
    }

    pub async fn sync(self) -> Vec<PathBuf> {
        eprintln!("syncing {} podcasts", &self.configs.len());

        let Some(longest_name) = self.configs.longest_name() else {
            return vec![];
        };

        let futures = self
            .configs
            .into_inner()
            .into_iter()
            .map(|(name, config)| {
                let client = Arc::clone(&self.client);
                let ui = DownloadBar::new(
                    name.clone(),
                    self.global_config.style(),
                    &self.mp,
                    longest_name,
                );
                let global_config = Arc::clone(&self.global_config);

                tokio::task::spawn(async move {
                    match Podcast::new(name, config, client, &ui, &global_config).await {
                        Ok(podcast) => podcast.sync(&ui).await,
                        Err(e) => {
                            ui.error(&e);
                            return vec![];
                        }
                    }
                })
            })
            .collect::<Vec<_>>();

        future::join_all(futures)
            .await
            .into_iter()
            .filter_map(Result::ok)
            .flatten()
            .collect()
    }
}

#[derive(Debug)]
pub struct RawPodcast(Map<String, serde_json::Value>);

impl RawPodcast {
    pub fn new(raw: serde_json::Map<String, serde_json::Value>) -> Self {
        Self(raw)
    }

    pub fn get_str(&self, key: &str) -> Option<&str> {
        utils::val_to_str(self.0.get(key)?)
    }

    pub fn title(&self) -> &str {
        self.get_str("title").unwrap()
    }

    pub fn author(&self) -> Option<&str> {
        let key = "itunes:author";
        self.get_str(&key)
    }

    pub fn categories(&self) -> Vec<&str> {
        let key = "itunes:category";
        match self.0.get(key).and_then(|x| x.as_array()) {
            Some(v) => v.iter().filter_map(utils::val_to_str).collect(),
            None => vec![],
        }
    }

    pub fn copyright(&self) -> Option<&str> {
        let inner = self.0.get("copyright")?;
        utils::val_to_str(&inner)
    }

    pub fn language(&self) -> Option<&str> {
        self.get_str("language")
    }

    pub fn image(&self) -> Option<&str> {
        let inner = self.0.get("image")?;
        utils::val_to_url(inner)
    }
}

#[derive(Debug)]
pub struct Podcast {
    episodes: Vec<Episode>,
    client: Arc<reqwest::Client>,
    mode: DownloadMode,
}

use crate::episode::EpisodeAttributes;

impl Podcast {
    pub async fn new(
        name: String,
        config: PodcastConfig,
        client: Arc<reqwest::Client>,
        ui: &DownloadBar,
        global_config: &GlobalConfig,
    ) -> Result<Podcast, String> {
        ui.fetching();
        let Some(xml_string) = utils::download_text(&client, &config.url, ui).await else {
            return Err("failed to download xml-file".to_string());
        };

        let Some((raw_podcast, raw_episodes)) = xml_to_value(&xml_string) else {
            return Err("failed to parse xml".to_string());
        };

        let mut episodes = vec![];
        for (index, episode) in raw_episodes.into_iter().enumerate() {
            if let Some(attrs) = EpisodeAttributes::new(episode) {
                let data = EvalData::new(&name, &raw_podcast, &attrs);
                let config = Config::new(global_config, &config, data);
                let tags = tags::extract_tags_from_raw(&raw_podcast, &attrs).await;

                let url = if let Some(url) = attrs.image().or(raw_podcast.image()) {
                    Some(url.to_string())
                } else {
                    None
                };

                let mut episode = Episode::new(attrs, index, config, tags);
                episode.image_url = url;
                episodes.push(episode);
            };
        }

        episodes.sort_by_key(|ep| ep.attrs.published);

        Ok(Podcast {
            episodes,
            client,
            mode: DownloadMode::new(global_config, &config),
        })
    }

    pub async fn download_episode<'a>(
        &'a self,
        episode: &'a Episode,
        ui: &DownloadBar,
    ) -> Result<DownloadedEpisode<'a>, String> {
        let mut episode = episode.download(&self.client, ui).await;
        episode.process().await?;
        episode.run_download_hook();
        episode.mark_downloaded();
        Ok(episode)
    }

    pub async fn sync(self, ui: &DownloadBar) -> Vec<PathBuf> {
        ui.init();

        let episodes = self.pending_episodes();
        let mut downloaded = vec![];

        for (index, episode) in episodes.iter().enumerate() {
            ui.begin_download(&episode, index, episodes.len());
            match self.download_episode(episode, ui).await {
                Ok(downloaded_episode) => downloaded.push(downloaded_episode),
                Err(e) => {
                    ui.error(&e);
                    break;
                }
            };
        }

        let mut paths = vec![];

        ui.hook_status();
        for episode in &mut downloaded {
            episode.await_handle().await;
            paths.push(episode.path().to_path_buf());
        }

        ui.complete();
        paths
    }

    fn pending_episodes(&self) -> Vec<&Episode> {
        let qty = self.episodes.len();

        let mut pending: Vec<&Episode> = self
            .episodes
            .iter()
            .filter(|episode| episode.should_download(&self.mode, qty))
            .collect();

        // In backlog mode it makes more sense to download earliest episode first.
        // in standard mode, the most recent episodes are more relevant.
        match self.mode {
            DownloadMode::Backlog { .. } => {
                pending.sort_by_key(|ep| ep.index);
            }
            DownloadMode::Standard { .. } => {
                pending.sort_by_key(|ep| ep.index);
                pending.reverse();
            }
        }

        pending
    }
}
