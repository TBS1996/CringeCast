use crate::config::{Config, GlobalConfig, PodcastConfig};

use crate::config::DownloadMode;
use crate::episode::DownloadedEpisode;
use crate::episode::Episode;
use crate::utils::current_unix;
use crate::utils::get_guid;
use crate::utils::remove_xml_namespaces;
use crate::utils::truncate_string;
use crate::utils::Unix;
use crate::utils::NAMESPACE_ALTER;
use futures_util::StreamExt;
use id3::TagLike;
use indicatif::MultiProgress;
use indicatif::ProgressBar;
use indicatif::ProgressStyle;
use quickxml_to_serde::{xml_string_to_json, Config as XmlConfig};
use reqwest::Client;
use serde_json::Value;
use std::collections::HashMap;
use std::io::Write as IOWrite;
use std::path::Path;
use std::path::PathBuf;

fn xml_to_value(xml: &str) -> Value {
    let xml = remove_xml_namespaces(&xml, NAMESPACE_ALTER);
    let conf = XmlConfig::new_with_defaults();
    xml_string_to_json(xml, &conf).unwrap()
}

fn init_podcast_status(mp: &MultiProgress, name: &str) -> ProgressBar {
    let pb = mp.add(ProgressBar::new_spinner());
    pb.set_style(
        ProgressStyle::default_spinner()
            .template("{spinner:.green}  {msg}")
            .unwrap(),
    );
    pb.set_message(name.to_owned());
    pb.enable_steady_tick(std::time::Duration::from_millis(100));
    pb
}

#[derive(Debug)]
pub struct Podcast {
    /// The configured name in `podcasts.toml`.
    name: String,
    channel: rss::Channel,
    xml: serde_json::Value,
    config: Config,
    progress_bar: Option<ProgressBar>,
}

impl Podcast {
    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn config(&self) -> &Config {
        &self.config
    }

    fn download_folder(&self) -> PathBuf {
        let download_pattern = &self.config.download_path;
        let evaluated = self.evaluate_pattern(download_pattern, None, None);
        let path = PathBuf::from(evaluated);
        std::fs::create_dir_all(&path).unwrap();
        path
    }

    pub async fn load_all(
        global_config: &GlobalConfig,
        filter: Option<&regex::Regex>,
        mp: Option<&MultiProgress>,
    ) -> Vec<Self> {
        let configs: HashMap<String, PodcastConfig> = {
            let path = crate::utils::podcasts_toml();
            if !path.exists() {
                eprintln!("You need to create a 'podcasts.toml' file to get started");
                std::process::exit(1);
            }
            let config_str = std::fs::read_to_string(path).unwrap();
            toml::from_str(&config_str).unwrap()
        };

        let podcast_qty = configs.len();
        let mut podcasts = vec![];
        for (name, config) in configs {
            if let Some(re) = filter {
                if !re.is_match(&name) {
                    continue;
                }
            }

            let config = Config::new(&global_config, config);
            let xml_string = Self::load_xml(&config.url).await;
            let channel = rss::Channel::read_from(xml_string.as_bytes()).unwrap();
            let xml_value = xml_to_value(&xml_string);
            let progress_bar = match mp {
                Some(mp) => Some(init_podcast_status(mp, &name)),
                None => None,
            };

            podcasts.push(Self {
                name,
                channel,
                xml: xml_value,
                config,
                progress_bar,
            });
        }

        eprintln!("syncing {}/{} podcasts", podcasts.len(), podcast_qty);

        podcasts
    }

    fn get_text_attribute(&self, key: &str) -> Option<&str> {
        let rss = self.xml.get("rss").unwrap();
        let channel = rss.get("channel").unwrap();
        channel.get(key).unwrap().as_str()
    }

    fn episodes(&self) -> Vec<Episode<'_>> {
        let mut vec = vec![];

        let mut map = HashMap::<&str, &serde_json::Map<String, serde_json::Value>>::new();

        let rss = self.xml.get("rss").unwrap();
        let channel = rss.get("channel").unwrap();
        let raw_items = channel
            .get("item")
            .expect("items not found")
            .as_array()
            .unwrap();

        for item in raw_items {
            let item = item.as_object().unwrap();
            let guid = get_guid(item);
            map.insert(guid, item);
        }

        for item in self.channel.items() {
            let Some(guid) = item.guid() else { continue };
            let obj = map.get(guid.value()).unwrap();

            // in case the episodes are not chronological we put all indices as zero and then
            // sort by published date and set index.
            if let Some(episode) = Episode::new(&item, 0, obj) {
                vec.push(episode);
            }
        }

        vec.sort_by_key(|episode| episode.published);

        let mut index = 0;
        for episode in &mut vec {
            episode.index = index;
            index += 1;
        }

        vec
    }

    fn rename_file(&self, file: &Path, tags: Option<&id3::Tag>, episode: &Episode) -> PathBuf {
        let pattern = &self.config.name_pattern;
        let result = self.evaluate_pattern(pattern, tags, Some(episode));

        let new_name = match file.extension() {
            Some(extension) => {
                let mut new_path = file.with_file_name(result);
                new_path.set_extension(extension);
                new_path
            }
            None => file.with_file_name(result),
        };

        std::fs::rename(file, &new_name).unwrap();
        new_name
    }

    fn evaluate_pattern(
        &self,
        pattern: &str,
        tags: Option<&id3::Tag>,
        episode: Option<&Episode>,
    ) -> String {
        let null = "<value not found>";
        let re = regex::Regex::new(r"\{([^\}]+)\}").unwrap();

        let mut result = String::new();
        let mut last_end = 0;

        use chrono::TimeZone;

        for cap in re.captures_iter(&pattern) {
            let match_range = cap.get(0).unwrap().range();
            let key = &cap[1];

            result.push_str(&pattern[last_end..match_range.start]);

            let replacement = match key {
                date if date.starts_with("pubdate::") && episode.is_some() => {
                    let episode = episode.unwrap();
                    let datetime = chrono::Utc.timestamp_opt(episode.published, 0).unwrap();
                    let (_, format) = date.split_once("::").unwrap();
                    if format == "unix" {
                        episode.published.to_string()
                    } else {
                        datetime.format(format).to_string()
                    }
                }
                id3 if id3.starts_with("id3::") && tags.is_some() => {
                    let (_, tag) = id3.split_once("::").unwrap();
                    tags.unwrap()
                        .get(tag)
                        .and_then(|tag| tag.content().text())
                        .unwrap_or(null)
                        .to_string()
                }
                rss if rss.starts_with("rss::episode::") && episode.is_some() => {
                    let episode = episode.unwrap();
                    let (_, key) = rss.split_once("episode::").unwrap();

                    let key = key.replace(":", NAMESPACE_ALTER);
                    episode.get_text_value(&key).unwrap_or(null).to_string()
                }
                rss if rss.starts_with("rss::channel::") => {
                    let (_, key) = rss.split_once("channel::").unwrap();

                    let key = key.replace(":", NAMESPACE_ALTER);
                    self.get_text_attribute(&key).unwrap_or(null).to_string()
                }

                "guid" if episode.is_some() => episode.unwrap().guid.to_string(),
                "url" if episode.is_some() => episode.unwrap().url.to_string(),
                "podname" => self.name.clone(),
                "appname" => crate::APPNAME.to_string(),
                "home" => dirs::home_dir()
                    .unwrap()
                    .as_os_str()
                    .to_str()
                    .unwrap()
                    .to_owned(),
                invalid_tag => {
                    eprintln!("invalid tag configured: {}", invalid_tag);
                    std::process::exit(1);
                }
            };

            result.push_str(&replacement);

            last_end = match_range.end;
        }

        result.push_str(&pattern[last_end..]);
        result
    }

    async fn load_xml(url: &str) -> String {
        let response = reqwest::Client::new()
            .get(url)
            .header(
                "User-Agent",
                "Mozilla/5.0 (X11; Linux x86_64; rv:124.0) Gecko/20100101 Firefox/124.0",
            )
            .send()
            .await
            .unwrap();

        if response.status().is_success() {
            let xml = response.text().await.unwrap();

            xml
        } else {
            panic!("failed to get response or smth");
        }
    }

    fn should_download(
        &self,
        episode: &Episode,
        latest_episode: usize,
        downloaded: &DownloadedEpisodes,
    ) -> bool {
        let id = self.get_id(episode);

        if downloaded.0.contains_key(&id) {
            return false;
        }

        match &self.config.mode {
            DownloadMode::Backlog { start, interval } => {
                let days_passed = (current_unix() - start.as_secs() as i64) / 86400;
                let current_backlog_index = days_passed / interval;

                current_backlog_index >= episode.index as i64
            }

            DownloadMode::Standard {
                max_days,
                max_episodes,
                earliest_date,
            } => {
                let max_days_exceeded = || {
                    max_days.is_some_and(|max_days| {
                        (current_unix() - episode.published) > max_days as i64 * 86400
                    })
                };

                let max_episodes_exceeded = || {
                    max_episodes.is_some_and(|max_episodes| {
                        (latest_episode - max_episodes as usize) > episode.index
                    })
                };

                let episode_too_old = || {
                    earliest_date.as_ref().is_some_and(|date| {
                        chrono::DateTime::parse_from_rfc3339(&date)
                            .unwrap()
                            .timestamp()
                            > episode.published
                    })
                };

                !max_days_exceeded() && !max_episodes_exceeded() && !episode_too_old()
            }
        }
    }

    fn mark_downloaded(&self, episode: &Episode) {
        let path = self.download_folder();
        let id = self.get_id(episode);
        DownloadedEpisodes::append(&id, &path, &episode);
    }

    fn get_id(&self, episode: &Episode) -> String {
        let id_pattern = &self.config.id_pattern;
        self.evaluate_pattern(id_pattern, None, Some(episode))
            .replace(" ", "_")
    }

    fn pending_episodes(&self) -> Vec<Episode<'_>> {
        let download_folder = self.download_folder();
        let mut episodes = self.episodes();
        let episode_qty = episodes.len();
        let downloaded = DownloadedEpisodes::load(&download_folder);

        episodes = episodes
            .into_iter()
            .filter(|episode| self.should_download(episode, episode_qty, &downloaded))
            .collect();

        // In backlog mode it makes more sense to download earliest episode first.
        // in standard mode, the most recent episodes are seen as more relevant.
        match self.config.mode {
            DownloadMode::Backlog { .. } => {
                episodes.sort_by_key(|ep| ep.index);
            }
            DownloadMode::Standard { .. } => {
                episodes.sort_by_key(|ep| ep.index);
                episodes.reverse();
            }
        }

        episodes
    }

    fn set_download_style(&self) {
        if let Some(pb) = &self.progress_bar {
            pb.set_style(
                ProgressStyle::default_bar()
                    .template("{spinner:.green} {msg} {bar:15.cyan/blue} {bytes}/{total_bytes}")
                    .unwrap(),
            );
        }
    }

    fn show_download_info(
        &self,
        episode: &Episode,
        index: usize,
        longest_podcast_name: usize,
        episode_qty: usize,
    ) {
        if let Some(pb) = &self.progress_bar {
            let fitted_episode_title = {
                let title_length = 30;
                let padded = &format!("{:<width$}", episode.title, width = title_length);
                truncate_string(padded, title_length)
            };

            let msg = format!(
                "{:<podcast_width$} {}/{} {} ",
                &self.name,
                index + 1,
                episode_qty,
                &fitted_episode_title,
                podcast_width = longest_podcast_name + 3
            );

            pb.set_message(msg);
            pb.set_position(0);
        }
    }

    fn set_template(&self, style: &str) {
        if let Some(pb) = &self.progress_bar {
            pb.set_style(ProgressStyle::default_bar().template(style).unwrap());
        }
    }

    pub async fn download_episode<'a>(&self, episode: Episode<'a>) -> DownloadedEpisode<'a> {
        let partial_path = {
            let file_name = format!("{}.partial", episode.guid);
            self.download_folder().join(file_name)
        };

        let mut downloaded: u64 = 0;

        let mut file = if partial_path.exists() {
            use std::io::Seek;
            let mut file = std::fs::OpenOptions::new()
                .write(true)
                .open(&partial_path)
                .unwrap();
            downloaded = file.seek(std::io::SeekFrom::End(0)).unwrap();
            file
        } else {
            std::fs::File::create(&partial_path).unwrap()
        };

        let mut req_builder = Client::new().get(episode.url);

        if downloaded > 0 {
            let range_header_value = format!("bytes={}-", downloaded);
            req_builder = req_builder.header(reqwest::header::RANGE, range_header_value);
        }

        let response = req_builder.send().await.unwrap();
        let total_size = response.content_length().unwrap_or(0);

        let ext = {
            let content_type = response
                .headers()
                .get(reqwest::header::CONTENT_TYPE)
                .and_then(|ct| ct.to_str().ok())
                .unwrap_or("application/octet-stream");

            let extensions = mime_guess::get_mime_extensions_str(&content_type).unwrap();

            match extensions.contains(&"mp3") {
                true => "mp3",
                false => extensions.first().expect("extension not found."),
            }
        };

        if let Some(pb) = &self.progress_bar {
            pb.set_length(total_size);
            pb.set_position(downloaded);
        }

        let mut stream = response.bytes_stream();

        while let Some(item) = stream.next().await {
            let chunk = item.unwrap();
            file.write_all(&chunk).unwrap();
            downloaded = std::cmp::min(downloaded + (chunk.len() as u64), total_size);

            if let Some(pb) = &self.progress_bar {
                pb.set_position(downloaded);
            }
        }

        let path = {
            let mut path = partial_path.clone();
            path.set_extension(ext);
            path
        };

        std::fs::rename(partial_path, &path).unwrap();

        DownloadedEpisode {
            inner: episode,
            file,
            path,
        }
    }

    async fn normalize_episode(&self, episode: &mut DownloadedEpisode<'_>) {
        let file_path = &episode.path;
        let mp3_tags = (file_path.extension().unwrap() == "mp3").then_some(
            crate::tags::set_mp3_tags(
                &self.channel,
                &episode.inner,
                file_path,
                &self.config.id3_tags,
            )
            .await,
        );

        let new_path = self.rename_file(&file_path, mp3_tags.as_ref(), &episode.inner);
        episode.path = new_path;
    }

    fn run_download_hook(
        &self,
        episode: &DownloadedEpisode,
    ) -> Option<tokio::task::JoinHandle<()>> {
        let script_path = self.config.download_hook.clone()?;
        let path = episode.path.clone();

        let handle = tokio::task::spawn_blocking(move || {
            let _ = std::process::Command::new(script_path).arg(path).output();
        });

        Some(handle)
    }

    fn mark_complete(&self) {
        if let Some(pb) = &self.progress_bar {
            self.set_template("{msg}");
            let msg = format!("✅ {}", &self.name);
            pb.finish_with_message(msg);
        }
    }

    pub async fn sync(&self, longest_podcast_name: usize) -> Vec<PathBuf> {
        self.set_download_style();

        let episodes = self.pending_episodes();
        let episode_qty = episodes.len();

        let mut downloaded = vec![];
        let mut hook_handles = vec![];

        for (index, episode) in episodes.into_iter().enumerate() {
            self.show_download_info(&episode, index, longest_podcast_name, episode_qty);
            let mut downloaded_episode = self.download_episode(episode).await;
            self.normalize_episode(&mut downloaded_episode).await;
            self.mark_downloaded(&downloaded_episode.inner);
            hook_handles.extend(self.run_download_hook(&downloaded_episode));
            downloaded.push(downloaded_episode);
        }

        if !hook_handles.is_empty() {
            self.set_template("{spinner:.green} finishing up download hooks...");
            futures::future::join_all(hook_handles).await;
        }

        self.mark_complete();
        downloaded.into_iter().map(|episode| episode.path).collect()
    }
}

/// Keeps track of which episodes have already been downloaded.
#[derive(Debug, Default)]
struct DownloadedEpisodes(HashMap<String, Unix>);

impl DownloadedEpisodes {
    const FILENAME: &'static str = ".downloaded";

    fn load(path: &Path) -> Self {
        let path = path.join(Self::FILENAME);
        let s = match std::fs::read_to_string(path) {
            Ok(s) => s,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return Self::default();
            }
            e @ Err(_) => e.unwrap(),
        };

        let mut hashmap: HashMap<String, Unix> = HashMap::new();

        for line in s.trim().lines() {
            let mut parts = line.split_whitespace();
            if let (Some(id), Some(timestamp_str)) = (parts.next(), parts.next()) {
                let id = id.to_string();
                let timestamp = timestamp_str
                    .parse::<i64>()
                    .expect("Timestamp should be a valid i64");
                let timestamp = std::time::Duration::from_secs(timestamp as u64);

                hashmap.insert(id, timestamp);
            }
        }

        Self(hashmap)
    }

    fn append(id: &str, path: &Path, episode: &Episode) {
        let path = path.join(Self::FILENAME);
        use std::io::Write;

        let mut file = std::fs::OpenOptions::new()
            .append(true)
            .create(true)
            .open(path)
            .unwrap();

        writeln!(file, "{} {} \"{}\"", id, current_unix(), &episode.title).unwrap();
    }
}
