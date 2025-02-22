use serde::de::Deserialize;
use std::cmp::{max, min};
use std::collections::HashMap;
use std::process::Stdio;
use tokio::io::AsyncReadExt;
use tokio::process::Command;
use tokio::sync::mpsc;

use super::{BlockEvent, BlockMessage};
use crate::click::MouseButton;
use crate::config::SharedConfig;
use crate::errors::*;
use crate::formatting::value::Value;
use crate::formatting::FormatTemplate;
use crate::widget::{Spacing, State, Widget};

const FILTER: &[char] = &['[', ']', '%'];

#[derive(serde_derive::Deserialize, Debug, Clone)]
#[serde(deny_unknown_fields, default)]
pub struct SoundConfig {
    pub name: Option<String>,
    pub device: Option<String>,
    pub device_kind: DeviceKind,
    pub natural_mapping: bool,
    pub step_width: u32,
    pub format: FormatTemplate,
    pub show_volume_when_muted: bool,
    pub mappings: Option<HashMap<String, String>>,
    pub max_vol: Option<u32>,
}

impl Default for SoundConfig {
    fn default() -> Self {
        Self {
            name: None,
            device: None,
            device_kind: Default::default(),
            natural_mapping: false,
            step_width: 5,
            format: FormatTemplate::default(),
            show_volume_when_muted: false,
            mappings: None,
            max_vol: None,
        }
    }
}

pub async fn run(
    id: usize,
    block_config: toml::Value,
    shared_config: SharedConfig,
    message_sender: mpsc::Sender<BlockMessage>,
    mut events_reciever: mpsc::Receiver<BlockEvent>,
) -> Result<()> {
    let block_config = SoundConfig::deserialize(block_config).block_config_error("sound")?;
    let format = block_config.format.or_default("{volume}")?;
    let mut text = Widget::new(id, shared_config);

    let device_kind = block_config.device_kind;
    let icon = |volume: u32| -> String {
        let prefix = match device_kind {
            DeviceKind::Source => "microphone",
            DeviceKind::Sink => "volume",
        };

        let suffix = match volume {
            0 => "muted",
            1..=20 => "empty",
            21..=70 => "half",
            _ => "full",
        };

        format!("{}_{}", prefix, suffix)
    };

    let step_width = block_config.step_width.clamp(0, 50) as i32;

    let mut device = AlsaSoundDevice::new(
        block_config.name.unwrap_or_else(|| "Master".into()),
        block_config.device.unwrap_or_else(|| "default".into()),
        block_config.natural_mapping,
    )
    .await?;

    let mut monitor = Command::new("stdbuf")
        .args(&["-oL", "alsactl", "monitor"])
        .stdout(Stdio::piped())
        .spawn()
        .block_error("sound", "Failed to start alsactl monitor")?
        .stdout
        .block_error("sound", "Failed to pipe alsactl monitor output")?;
    let mut buffer = [0; 1024]; // Should be more than enough.

    loop {
        device.get_info().await?;
        let volume = device.volume();
        let mut output_name = device.output_name();

        if let Some(m) = &block_config.mappings {
            if let Some(mapped) = m.get(&output_name) {
                output_name = mapped.to_string();
            }
        }

        text.set_text(format.render(&map! {
            "volume" => Value::from_integer(volume as i64).percents(),
            "output_name" => Value::from_string(output_name),
        })?);

        if device.muted() {
            text.set_icon(&icon(0))?;
            text.set_state(State::Warning);
            if !block_config.show_volume_when_muted {
                text.set_text((String::new(), None));
            }
        } else {
            text.set_icon(&icon(volume))?;
            text.set_spacing(Spacing::Normal);
            text.set_state(State::Idle);
        }

        message_sender
            .send(BlockMessage {
                id,
                widgets: vec![text.get_data()],
            })
            .await
            .internal_error("sound", "failed to send message")?;

        tokio::select! {
            _ = monitor.read(&mut buffer) => (),
            Some(BlockEvent::I3Bar(click)) = events_reciever.recv() => {
                match click.button {
                    MouseButton::Right => {
                        device.toggle().await?;
                    }
                    MouseButton::WheelUp => {
                        device.set_volume(step_width, block_config.max_vol).await?;
                    }
                    MouseButton::WheelDown => {
                        device.set_volume(-step_width, block_config.max_vol).await?;
                    }
                    _ => ()
                }
            }
        }
    }
}

struct AlsaSoundDevice {
    name: String,
    device: String,
    natural_mapping: bool,
    volume: u32,
    muted: bool,
}

impl AlsaSoundDevice {
    async fn new(name: String, device: String, natural_mapping: bool) -> Result<Self> {
        Ok(AlsaSoundDevice {
            name,
            device,
            natural_mapping,
            volume: 0,
            muted: false,
        })
    }

    fn volume(&self) -> u32 {
        self.volume
    }
    fn muted(&self) -> bool {
        self.muted
    }
    fn output_name(&self) -> String {
        self.name.clone()
    }

    async fn get_info(&mut self) -> Result<()> {
        let mut args = Vec::new();
        if self.natural_mapping {
            args.push("-M")
        };
        args.extend(&["-D", &self.device, "get", &self.name]);

        let output = Command::new("amixer")
            .args(&args)
            .output()
            .await
            .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_owned())
            .block_error("sound", "could not run amixer to get sound info")?;

        let last_line = &output
            .lines()
            .last()
            .block_error("sound", "could not get sound info")?;

        let mut last = last_line
            .split_whitespace()
            .filter(|x| x.starts_with('[') && !x.contains("dB"))
            .map(|s| s.trim_matches(FILTER));

        self.volume = last
            .next()
            .block_error("sound", "could not get volume")?
            .parse::<u32>()
            .block_error("sound", "could not parse volume to u32")?;

        self.muted = last.next().map(|muted| muted == "off").unwrap_or(false);

        Ok(())
    }

    async fn set_volume(&mut self, step: i32, max_vol: Option<u32>) -> Result<()> {
        let new_vol = max(0, self.volume as i32 + step) as u32;
        let capped_volume = if let Some(vol_cap) = max_vol {
            min(new_vol, vol_cap)
        } else {
            new_vol
        };
        let mut args = Vec::new();
        if self.natural_mapping {
            args.push("-M")
        };
        let vol_str = format!("{}%", capped_volume);
        args.extend(&["-D", &self.device, "set", &self.name, &vol_str]);

        Command::new("amixer")
            .args(&args)
            .output()
            .await
            .block_error("sound", "failed to set volume")?;

        self.volume = capped_volume;

        Ok(())
    }

    async fn toggle(&mut self) -> Result<()> {
        let mut args = Vec::new();
        if self.natural_mapping {
            args.push("-M")
        };
        args.extend(&["-D", &self.device, "set", &self.name, "toggle"]);

        Command::new("amixer")
            .args(&args)
            .output()
            .await
            .block_error("sound", "failed to toggle mute")?;

        self.muted = !self.muted;

        Ok(())
    }
}

#[derive(serde_derive::Deserialize, Clone, Copy, Debug, Eq, Hash, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum DeviceKind {
    Sink,
    Source,
}

impl Default for DeviceKind {
    fn default() -> Self {
        Self::Sink
    }
}
