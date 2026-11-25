//! Windows Core Audio (WASAPI) integration:
//! - Default output device name, icon kind, master volume, and mute state.
//! - List of all active playback endpoints (`eRender`) and instant default output switching via `IPolicyConfig`.
//! - Per-application audio sessions (`IAudioSessionManager2`, `ISimpleAudioVolume`, `IAudioMeterInformation`).
//! - Communication mode detection (Teams, Discord, Zoom, etc. or active microphone capture session).

use crate::diag;
use crate::i18n::t;
use crate::wide;
use std::collections::HashMap;
use std::ffi::c_void;
use std::ptr::null;
use std::sync::atomic::{AtomicU32, Ordering};
use windows::core::{IUnknown, IUnknown_Vtbl, Interface, GUID, HRESULT, PCWSTR};
use windows::Win32::Devices::FunctionDiscovery::PKEY_Device_FriendlyName;
use windows::Win32::Foundation::{E_NOINTERFACE, E_POINTER, S_OK};
use windows::Win32::Media::Audio::Endpoints::{
    IAudioEndpointVolume, IAudioEndpointVolumeCallback, IAudioEndpointVolumeCallback_Vtbl,
    IAudioMeterInformation,
};
use windows::Win32::Media::Audio::*;
use windows::Win32::System::Com::StructuredStorage::{
    PropVariantToStringAlloc, PropVariantToUInt32,
};
use windows::Win32::System::Com::{
    CoCreateInstance, CLSCTX_ALL, CLSCTX_INPROC_SERVER, STGM_READ,
};
use windows::Win32::System::Threading::{
    OpenProcess, QueryFullProcessImageNameW, PROCESS_NAME_WIN32, PROCESS_QUERY_LIMITED_INFORMATION,
};
use windows::Win32::UI::Shell::PropertiesSystem::PROPERTYKEY;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum DeviceKind {
    Headphones,
    Headset,
    Speakers,
    HdmiDisplay,
    Bluetooth,
    Generic,
}

impl DeviceKind {
    pub fn from_name_and_form(name: &str, form_factor: u32) -> DeviceKind {
        let lower = name.to_ascii_lowercase();
        // Check explicit keywords first because drivers often report generic EndpointFormFactor
        if [
            "airpods", "buds", "wh-1000", "wf-1000", "bose qc",
        ]
        .iter()
        .any(|k| lower.contains(k))
        {
            if lower.contains("headset") || lower.contains("hands-free") {
                return DeviceKind::Headset;
            }
            return DeviceKind::Headphones;
        }
        if lower.contains("bluetooth") && !lower.contains("headset") && !lower.contains("headphone")
        {
            return DeviceKind::Bluetooth;
        }
        if [
            "headset",
            "arctis",
            "hyperx",
            "cloud",
            "kraken",
            "blackshark",
            "g pro x",
            "void",
            "hs80",
            "virtuoso",
        ]
        .iter()
        .any(|k| lower.contains(k))
        {
            return DeviceKind::Headset;
        }
        if ["headphone", "fones", "earphone", "auricular", "kopfhörer"]
            .iter()
            .any(|k| lower.contains(k))
        {
            return DeviceKind::Headphones;
        }
        if [
            "hdmi",
            "displayport",
            "nvidia high definition",
            "amd high definition",
            "monitor",
            "tv",
            "lg ultragear",
            "odyssey",
            "dell ",
            "benq",
            "AOC",
        ]
        .iter()
        .any(|k| lower.contains(&k.to_ascii_lowercase()))
        {
            return DeviceKind::HdmiDisplay;
        }
        if ["speaker", "alto-falante", "altavoces", "haut-parleur", "lautsprecher", "realtek"]
            .iter()
            .any(|k| lower.contains(k))
        {
            return DeviceKind::Speakers;
        }

        // Fall back to Windows EndpointFormFactor enum values:
        // 1 = Speakers, 3 = Headphones, 5 = Headset, 6 = Handset, 8 = DigitalAudioDisplayDevice (HDMI)
        match form_factor {
            3 => DeviceKind::Headphones,
            5 | 6 => DeviceKind::Headset,
            8 => DeviceKind::HdmiDisplay,
            1 => DeviceKind::Speakers,
            _ => DeviceKind::Generic,
        }
    }

    /// Concise display name for the taskbar so it doesn't eat half the bar.
    /// E.g. "Fones de ouvido (2- Realtek(R) Audio)" -> "Fones de ouvido" or "PRO X Wireless".
    pub fn short_label(full_name: &str) -> String {
        let trimmed = full_name.trim();
        // If format is "Speakers (PRO X Wireless Gaming Headset)", check if inside parens is a brand/device
        if let (Some(open), Some(close)) = (trimmed.find('('), trimmed.rfind(')')) {
            if close > open + 1 {
                let prefix = trimmed[..open].trim();
                let inside = trimmed[open + 1..close].trim();
                // Strip leading USB index like "2- "
                let inside_clean = inside
                    .trim_start_matches(|c: char| c.is_ascii_digit() || c == '-' || c == ' ')
                    .trim();
                let generic_prefixes = [
                    "speakers",
                    "headphones",
                    "headset",
                    "alto-falantes",
                    "fones de ouvido",
                    "auriculares",
                    "altavoces",
                    "haut-parleurs",
                    "casque",
                    "lautsprecher",
                    "kopfhörer",
                ];
                let generic_drivers = [
                    "realtek",
                    "high definition audio",
                    "usb audio",
                    "amd high definition",
                    "nvidia high definition",
                ];
                let prefix_is_generic = generic_prefixes
                    .iter()
                    .any(|g| prefix.to_ascii_lowercase().starts_with(g));
                let inside_is_generic_driver = generic_drivers
                    .iter()
                    .any(|g| inside_clean.to_ascii_lowercase().contains(g));

                let chosen = if prefix_is_generic && !inside_is_generic_driver && !inside_clean.is_empty() {
                    inside_clean
                } else if !prefix.is_empty() {
                    prefix
                } else {
                    inside_clean
                };
                return truncate_chars(chosen, 18);
            }
        }
        truncate_chars(trimmed, 18)
    }
}

fn truncate_chars(s: &str, max_chars: usize) -> String {
    let count = s.chars().count();
    if count <= max_chars {
        s.to_string()
    } else {
        let mut out: String = s.chars().take(max_chars.saturating_sub(1)).collect();
        out.push('…');
        out
    }
}

#[derive(Clone, Debug)]
pub struct OutputDevice {
    pub id: String,
    pub name: String,
    pub short_name: String,
    pub kind: DeviceKind,
    pub is_default: bool,
}

#[derive(Clone, Debug)]
pub struct InputDevice {
    pub id: String,
    pub name: String,
    pub is_default: bool,
}

#[derive(Clone, Debug)]
pub struct AppSession {
    pub key: String,
    pub name: String,
    pub exe_path: Option<String>,
    pub volume: u8,
    pub muted: bool,
    pub peak: f32,
    pub is_active: bool,
    pub is_comm_app: bool,
    pub is_system_sounds: bool,
}

#[derive(Clone, Debug)]
pub struct AudioSnapshot {
    pub master_volume: u8,
    pub master_muted: bool,
    pub device_name: String,
    pub device_short_name: String,
    pub device_kind: DeviceKind,
    pub outputs: Vec<OutputDevice>,
    pub inputs: Vec<InputDevice>,
    pub sessions: Vec<AppSession>,
    /// True when a communication app (Discord, Teams, Zoom, Meet, Slack, etc.) is actively streaming
    /// or an application is actively capturing from the microphone.
    pub in_communication: bool,
    /// Name of the app currently in a call/communication, if known.
    pub comm_app_name: Option<String>,
}

impl Default for AudioSnapshot {
    fn default() -> Self {
        Self {
            master_volume: 0,
            master_muted: false,
            device_name: String::from("Audio"),
            device_short_name: String::from("Audio"),
            device_kind: DeviceKind::Speakers,
            outputs: Vec::new(),
            inputs: Vec::new(),
            sessions: Vec::new(),
            in_communication: false,
            comm_app_name: None,
        }
    }
}

pub fn demo_snapshot() -> AudioSnapshot {
    AudioSnapshot {
        master_volume: 74,
        master_muted: false,
        device_name: "Headset (PRO X Wireless Gaming Headset)".into(),
        device_short_name: "PRO X Wireless".into(),
        device_kind: DeviceKind::Headset,
        outputs: vec![
            OutputDevice {
                id: "demo-headset".into(),
                name: "Headset (PRO X Wireless Gaming Headset)".into(),
                short_name: "PRO X Wireless".into(),
                kind: DeviceKind::Headset,
                is_default: true,
            },
            OutputDevice {
                id: "demo-speakers".into(),
                name: "Speakers (Realtek(R) Audio)".into(),
                short_name: "Speakers".into(),
                kind: DeviceKind::Speakers,
                is_default: false,
            },
            OutputDevice {
                id: "demo-hdmi".into(),
                name: "LG UltraGear (NVIDIA High Definition Audio)".into(),
                short_name: "LG UltraGear".into(),
                kind: DeviceKind::HdmiDisplay,
                is_default: false,
            },
        ],
        inputs: vec![
            InputDevice {
                id: "demo-mic-1".into(),
                name: "Microphone (PRO X Wireless Gaming Headset)".into(),
                is_default: true,
            },
            InputDevice {
                id: "demo-mic-2".into(),
                name: "Microphone Array (Realtek(R) Audio)".into(),
                is_default: false,
            },
        ],
        sessions: vec![
            AppSession {
                key: "discord".into(),
                name: "Discord".into(),
                exe_path: None,
                volume: 100,
                muted: false,
                peak: 0.42,
                is_active: true,
                is_comm_app: true,
                is_system_sounds: false,
            },
            AppSession {
                key: "spotify".into(),
                name: "Spotify".into(),
                exe_path: None,
                volume: 65,
                muted: false,
                peak: 0.68,
                is_active: true,
                is_comm_app: false,
                is_system_sounds: false,
            },
            AppSession {
                key: "chrome".into(),
                name: "Google Chrome".into(),
                exe_path: None,
                volume: 80,
                muted: false,
                peak: 0.0,
                is_active: false,
                is_comm_app: false,
                is_system_sounds: false,
            },
            AppSession {
                key: "sys".into(),
                name: t().mixer_system_sounds.into(),
                exe_path: None,
                volume: 40,
                muted: false,
                peak: 0.0,
                is_active: false,
                is_comm_app: false,
                is_system_sounds: true,
            },
        ],
        in_communication: true,
        comm_app_name: Some("Discord".into()),
    }
}

/// PKEY_AudioEndpoint_FormFactor: {1DA5D803-D492-4EDD-8C23-E0C0FFEE7F0E}, pid 0
const PKEY_ENDPOINT_FORMFACTOR: PROPERTYKEY = PROPERTYKEY {
    fmtid: GUID::from_u128(0x1da5d803_d492_4edd_8c23_e0c0ffee7f0e),
    pid: 0,
};

const KNOWN_COMM_APPS: &[(&str, &str)] = &[
    ("discord", "Discord"),
    ("ms-teams", "Microsoft Teams"),
    ("teams", "Microsoft Teams"),
    ("zoom", "Zoom"),
    ("slack", "Slack"),
    ("skype", "Skype"),
    ("webex", "Webex"),
    ("telegram", "Telegram"),
    ("whatsapp", "WhatsApp"),
    ("facetime", "FaceTime"),
    ("mumble", "Mumble"),
    ("ts3client", "TeamSpeak"),
    ("teamspeak", "TeamSpeak"),
];

/// Lightweight refresh used by the mixer flyout's 120 ms timer: updates only per-app peak meters,
/// volumes, and mute flags without enumerating all playback/capture hardware endpoints or opening
/// `IPropertyStore` on every device.
pub fn refresh_sessions_and_peaks(snap: &mut AudioSnapshot) -> bool {
    unsafe {
        let Ok(enumerator): Result<IMMDeviceEnumerator, _> =
            CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_INPROC_SERVER)
        else {
            return false;
        };
        let Ok(default_dev) = enumerator.GetDefaultAudioEndpoint(eRender, eConsole) else {
            return false;
        };
        if let Ok(ep_vol) = default_dev.Activate::<IAudioEndpointVolume>(CLSCTX_ALL, None) {
            let vol_scalar = ep_vol.GetMasterVolumeLevelScalar().unwrap_or(0.0);
            snap.master_volume = (vol_scalar.clamp(0.0, 1.0) * 100.0).round() as u8;
            snap.master_muted = ep_vol.GetMute().map(|b| b.as_bool()).unwrap_or(false);
        }
        snap.sessions = read_render_sessions(&default_dev);
        true
    }
}

pub fn snapshot() -> Option<AudioSnapshot> {
    unsafe {
        let enumerator: IMMDeviceEnumerator =
            CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_INPROC_SERVER).ok()?;
        snapshot_with_enumerator(&enumerator)
    }
}

pub fn snapshot_with_enumerator(enumerator: &IMMDeviceEnumerator) -> Option<AudioSnapshot> {
    unsafe {
        // 1. Default playback endpoint
        let default_dev = enumerator.GetDefaultAudioEndpoint(eRender, eConsole).ok()?;
        let default_id = device_id(&default_dev).unwrap_or_default();
        let (default_name, default_kind) = device_name_and_kind(&default_dev);
        let default_short = DeviceKind::short_label(&default_name);

        // 2. Master volume & mute
        let ep_vol: IAudioEndpointVolume = default_dev.Activate(CLSCTX_ALL, None).ok()?;
        let vol_scalar = ep_vol.GetMasterVolumeLevelScalar().unwrap_or(0.0);
        let master_volume = (vol_scalar.clamp(0.0, 1.0) * 100.0).round() as u8;
        let master_muted = ep_vol.GetMute().map(|b| b.as_bool()).unwrap_or(false);

        // 3. All active playback endpoints
        let mut outputs = Vec::new();
        if let Ok(coll) = enumerator.EnumAudioEndpoints(eRender, DEVICE_STATE_ACTIVE) {
            let count = coll.GetCount().unwrap_or(0);
            for i in 0..count {
                if let Ok(dev) = coll.Item(i) {
                    let id = device_id(&dev).unwrap_or_default();
                    let (name, kind) = device_name_and_kind(&dev);
                    let short_name = DeviceKind::short_label(&name);
                    let is_default = id == default_id;
                    outputs.push(OutputDevice {
                        id,
                        name,
                        short_name,
                        kind,
                        is_default,
                    });
                }
            }
        }

        // 3b. All active capture (microphone) endpoints
        let default_mic_id = enumerator
            .GetDefaultAudioEndpoint(eCapture, eConsole)
            .ok()
            .and_then(|d| device_id(&d))
            .unwrap_or_default();
        let mut inputs = Vec::new();
        if let Ok(coll) = enumerator.EnumAudioEndpoints(eCapture, DEVICE_STATE_ACTIVE) {
            let count = coll.GetCount().unwrap_or(0);
            for i in 0..count {
                if let Ok(dev) = coll.Item(i) {
                    let id = device_id(&dev).unwrap_or_default();
                    let (name, _) = device_name_and_kind(&dev);
                    let is_default = id == default_mic_id;
                    inputs.push(InputDevice {
                        id,
                        name,
                        is_default,
                    });
                }
            }
        }

        // 4. Per-app audio sessions on the default playback device
        let mut sessions = read_render_sessions(&default_dev);

        // 5. Check communication status:
        //    (a) Any active microphone capture session (eCapture)
        //    (b) Any active render session belonging to a known communication app (Teams/Discord/Zoom/etc.)
        //    (c) Differences between eConsole and eCommunications default session activity
        let mut comm_app_name = check_active_capture_sessions(enumerator);
        for s in &mut sessions {
            if s.is_comm_app && (s.is_active || s.peak > 0.005) && comm_app_name.is_none() {
                comm_app_name = Some(s.name.clone());
            }
        }
        // Also check the eCommunications endpoint if it differs from eConsole (e.g. headset chat channel)
        if comm_app_name.is_none() {
            if let Ok(comm_dev) = enumerator.GetDefaultAudioEndpoint(eRender, eCommunications) {
                let comm_id = device_id(&comm_dev).unwrap_or_default();
                if comm_id != default_id {
                    for s in read_render_sessions(&comm_dev) {
                        if s.is_active && (s.is_comm_app || s.peak > 0.01) {
                            comm_app_name = Some(s.name);
                            break;
                        }
                    }
                }
            }
        }

        let in_communication = comm_app_name.is_some();

        diag::event(
            "audio",
            format!(
                "default='{}' ({:?}) vol={}% muted={} outputs={} apps={} comm={}",
                default_short,
                default_kind,
                master_volume,
                master_muted,
                outputs.len(),
                sessions.len(),
                in_communication
            ),
        );

        Some(AudioSnapshot {
            master_volume,
            master_muted,
            device_name: default_name,
            device_short_name: default_short,
            device_kind: default_kind,
            outputs,
            inputs,
            sessions,
            in_communication,
            comm_app_name,
        })
    }
}

unsafe fn read_render_sessions(device: &IMMDevice) -> Vec<AppSession> {
    let mut by_key: HashMap<String, AppSession> = HashMap::new();
    let Ok(mgr): Result<IAudioSessionManager2, _> = device.Activate(CLSCTX_ALL, None) else {
        return Vec::new();
    };
    let Ok(enumerator) = mgr.GetSessionEnumerator() else {
        return Vec::new();
    };
    let count = enumerator.GetCount().unwrap_or(0);

    for i in 0..count {
        let Ok(ctrl) = enumerator.GetSession(i) else {
            continue;
        };
        let state = ctrl.GetState().unwrap_or(AudioSessionStateExpired);
        if state == AudioSessionStateExpired {
            continue;
        }
        let is_active = state == AudioSessionStateActive;

        let ctrl2: Option<IAudioSessionControl2> = windows::core::Interface::cast(&ctrl).ok();
        let pid = ctrl2.as_ref().and_then(|c| c.GetProcessId().ok()).unwrap_or(0);
        let display_name = ctrl
            .GetDisplayName()
            .ok()
            .and_then(|p| pwstr_to_string(p.0))
            .filter(|s| !s.trim().is_empty() && !s.starts_with("@%"));

        let is_system_sounds = ctrl2
            .as_ref()
            .is_some_and(|c| c.IsSystemSoundsSession() == windows::Win32::Foundation::S_OK)
            || display_name
                .as_deref()
                .is_some_and(|d| d.contains("AudioSrv.Dll"));

        let (proc_name, exe_stem, exe_path) = if is_system_sounds || pid == 0 {
            (t().mixer_system_sounds.to_string(), String::from("system"), None)
        } else {
            match process_display_name(pid) {
                Some((_, stem, _)) if stem == "svchost" || stem == "audiodg" => {
                    (t().mixer_system_sounds.to_string(), String::from("system"), None)
                }
                Some((pretty, stem, path)) => (pretty, stem, Some(path)),
                None => {
                    // Process already exited; skip ghost session
                    if display_name.is_none() {
                        continue;
                    }
                    let dn = display_name.clone().unwrap_or_default();
                    (dn.clone(), dn.to_ascii_lowercase(), None)
                }
            }
        };
        let is_system_sounds = exe_stem == "system";

        let name = display_name.unwrap_or(proc_name);
        let is_comm_app = KNOWN_COMM_APPS
            .iter()
            .any(|(k, _)| exe_stem.contains(k) || name.to_ascii_lowercase().contains(k));

        let simple_vol: Option<ISimpleAudioVolume> = windows::core::Interface::cast(&ctrl).ok();
        let volume = simple_vol
            .as_ref()
            .and_then(|v| v.GetMasterVolume().ok())
            .map(|f| (f.clamp(0.0, 1.0) * 100.0).round() as u8)
            .unwrap_or(100);
        let muted = simple_vol
            .as_ref()
            .and_then(|v| v.GetMute().ok())
            .map(|b| b.as_bool())
            .unwrap_or(false);

        let meter: Option<IAudioMeterInformation> = windows::core::Interface::cast(&ctrl).ok();
        let peak = meter
            .as_ref()
            .and_then(|m| m.GetPeakValue().ok())
            .unwrap_or(0.0)
            .clamp(0.0, 1.0);

        // Group sessions belonging to the same executable (e.g. Chrome or Discord multiple audio streams)
        let key = if is_system_sounds {
            String::from("sys:0")
        } else {
            format!("app:{exe_stem}")
        };

        by_key
            .entry(key.clone())
            .and_modify(|existing| {
                existing.is_active |= is_active;
                existing.peak = existing.peak.max(peak);
                if existing.exe_path.is_none() && exe_path.is_some() {
                    existing.exe_path = exe_path.clone();
                }
                if is_active {
                    existing.volume = volume;
                    existing.muted = muted;
                }
            })
            .or_insert(AppSession {
                key,
                name,
                exe_path,
                volume,
                muted,
                peak,
                is_active,
                is_comm_app,
                is_system_sounds,
            });
    }

    let mut list: Vec<AppSession> = by_key.into_values().collect();
    // Sort order: communication apps first, then currently playing apps, then regular apps alphabetically, system sounds last
    list.sort_by(|a, b| {
        a.is_system_sounds
            .cmp(&b.is_system_sounds)
            .then_with(|| b.is_comm_app.cmp(&a.is_comm_app))
            .then_with(|| b.is_active.cmp(&a.is_active))
            .then_with(|| a.name.to_ascii_lowercase().cmp(&b.name.to_ascii_lowercase()))
    });
    list
}

/// Checks if any application is actively recording from the default microphone (`eCapture`).
/// Returns the application's friendly name if a microphone session is `AudioSessionStateActive`.
unsafe fn check_active_capture_sessions(enumerator: &IMMDeviceEnumerator) -> Option<String> {
    for role in [eCommunications, eConsole] {
        let Ok(cap_dev) = enumerator.GetDefaultAudioEndpoint(eCapture, role) else {
            continue;
        };
        let Ok(mgr): Result<IAudioSessionManager2, _> = cap_dev.Activate(CLSCTX_ALL, None) else {
            continue;
        };
        let Ok(sess_enum) = mgr.GetSessionEnumerator() else {
            continue;
        };
        let count = sess_enum.GetCount().unwrap_or(0);
        for i in 0..count {
            let Ok(ctrl) = sess_enum.GetSession(i) else {
                continue;
            };
            if ctrl.GetState().unwrap_or(AudioSessionStateInactive) != AudioSessionStateActive {
                continue;
            }
            let ctrl2: Option<IAudioSessionControl2> = windows::core::Interface::cast(&ctrl).ok();
            let pid = ctrl2.as_ref().and_then(|c| c.GetProcessId().ok()).unwrap_or(0);
            if pid == 0 {
                continue;
            }
            if let Some((pretty, _, _)) = process_display_name(pid) {
                return Some(pretty);
            }
        }
    }
    None
}

/// Adjusts the master volume by `delta` percentage points (e.g. +2 or -2) or sets it directly.
pub fn set_master_volume(percent: u8) {
    unsafe {
        let Ok(enumerator): Result<IMMDeviceEnumerator, _> =
            CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_INPROC_SERVER)
        else {
            return;
        };
        let Ok(dev) = enumerator.GetDefaultAudioEndpoint(eRender, eConsole) else {
            return;
        };
        let Ok(ep_vol): Result<IAudioEndpointVolume, _> = dev.Activate(CLSCTX_ALL, None) else {
            return;
        };
        let scalar = (percent.min(100) as f32) / 100.0;
        let _ = ep_vol.SetMasterVolumeLevelScalar(scalar, null());
        if percent > 0 {
            // Unmute automatically if the user drags volume up from muted
            let _ = ep_vol.SetMute(false, null());
        }
    }
}

pub fn step_master_volume(delta: i32) {
    unsafe {
        let Ok(enumerator): Result<IMMDeviceEnumerator, _> =
            CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_INPROC_SERVER)
        else {
            return;
        };
        let Ok(dev) = enumerator.GetDefaultAudioEndpoint(eRender, eConsole) else {
            return;
        };
        let Ok(ep_vol): Result<IAudioEndpointVolume, _> = dev.Activate(CLSCTX_ALL, None) else {
            return;
        };
        let cur = (ep_vol.GetMasterVolumeLevelScalar().unwrap_or(0.0) * 100.0).round() as i32;
        let next = (cur + delta).clamp(0, 100) as u8;
        let _ = ep_vol.SetMasterVolumeLevelScalar(next as f32 / 100.0, null());
        if delta > 0 && next > 0 {
            let _ = ep_vol.SetMute(false, null());
        }
    }
}

pub fn toggle_master_mute() {
    unsafe {
        let Ok(enumerator): Result<IMMDeviceEnumerator, _> =
            CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_INPROC_SERVER)
        else {
            return;
        };
        let Ok(dev) = enumerator.GetDefaultAudioEndpoint(eRender, eConsole) else {
            return;
        };
        let Ok(ep_vol): Result<IAudioEndpointVolume, _> = dev.Activate(CLSCTX_ALL, None) else {
            return;
        };
        let muted = ep_vol.GetMute().map(|b| b.as_bool()).unwrap_or(false);
        let _ = ep_vol.SetMute(!muted, null());
    }
}

/// Sets the volume (0..=100) for all audio sessions matching `session_key` (e.g. `app:spotify` or `sys:0`).
pub fn set_session_volume(session_key: &str, percent: u8) {
    for_each_matching_session(session_key, |vol| unsafe {
        let _ = vol.SetMasterVolume(percent.min(100) as f32 / 100.0, null());
        if percent > 0 {
            let _ = vol.SetMute(false, null());
        }
    });
}

/// Toggles mute for all audio sessions matching `session_key`.
pub fn toggle_session_mute(session_key: &str) {
    let mut target_mute: Option<bool> = None;
    for_each_matching_session(session_key, |vol| unsafe {
        let m = target_mute.get_or_insert_with(|| {
            !vol.GetMute().map(|b| b.as_bool()).unwrap_or(false)
        });
        let _ = vol.SetMute(*m, null());
    });
}

fn for_each_matching_session(session_key: &str, mut f: impl FnMut(&ISimpleAudioVolume)) {
    unsafe {
        let Ok(enumerator): Result<IMMDeviceEnumerator, _> =
            CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_INPROC_SERVER)
        else {
            return;
        };
        let Ok(dev) = enumerator.GetDefaultAudioEndpoint(eRender, eConsole) else {
            return;
        };
        let Ok(mgr): Result<IAudioSessionManager2, _> = dev.Activate(CLSCTX_ALL, None) else {
            return;
        };
        let Ok(sess_enum) = mgr.GetSessionEnumerator() else {
            return;
        };
        let count = sess_enum.GetCount().unwrap_or(0);
        for i in 0..count {
            let Ok(ctrl) = sess_enum.GetSession(i) else {
                continue;
            };
            let ctrl2: Option<IAudioSessionControl2> = windows::core::Interface::cast(&ctrl).ok();
            let is_sys = ctrl2
                .as_ref()
                .is_some_and(|c| c.IsSystemSoundsSession() == windows::Win32::Foundation::S_OK);
            let pid = ctrl2.as_ref().and_then(|c| c.GetProcessId().ok()).unwrap_or(0);

            let key = if is_sys || pid == 0 {
                String::from("sys:0")
            } else if let Some((_, stem, _)) = process_display_name(pid) {
                if stem == "svchost" || stem == "audiodg" {
                    String::from("sys:0")
                } else {
                    format!("app:{stem}")
                }
            } else {
                continue;
            };

            if key == session_key {
                if let Ok(vol) = windows::core::Interface::cast::<ISimpleAudioVolume>(&ctrl) {
                    f(&vol);
                }
            }
        }
    }
}

/// Switches the default Windows audio output or input endpoint across Console, Multimedia, and Communications roles
/// using the undocumented `IPolicyConfig` COM interface (supported on Windows 7, 10, and 11).
pub fn set_default_output_device(endpoint_id: &str) -> bool {
    const CLSID_POLICY_CONFIG_CLIENT: GUID =
        GUID::from_u128(0x870af99c_171d_4f9e_af0d_e63df40c2bc9);

    #[repr(transparent)]
    #[derive(Clone)]
    struct IPolicyConfig(IUnknown);

    #[repr(C)]
    struct IPolicyConfig_Vtbl {
        base__: IUnknown_Vtbl,
        get_mix_format: *const c_void,
        get_device_format: *const c_void,
        reset_device_format: *const c_void,
        set_device_format: *const c_void,
        get_processing_period: *const c_void,
        set_processing_period: *const c_void,
        get_share_mode: *const c_void,
        set_share_mode: *const c_void,
        get_property_value: *const c_void,
        set_property_value: *const c_void,
        set_default_endpoint: unsafe extern "system" fn(*mut c_void, PCWSTR, i32) -> HRESULT,
    }

    unsafe impl Interface for IPolicyConfig {
        type Vtable = IPolicyConfig_Vtbl;
        const IID: GUID = GUID::from_u128(0xf8679f50_850a_41cf_9c72_430f290290c8);
    }

    unsafe {
        let Ok(policy): Result<IPolicyConfig, _> =
            CoCreateInstance(&CLSID_POLICY_CONFIG_CLIENT, None, CLSCTX_ALL)
        else {
            return false;
        };
        let id_w = wide(endpoint_id);
        let pcw = PCWSTR(id_w.as_ptr());

        // eConsole = 0, eMultimedia = 1, eCommunications = 2
        let vtbl = Interface::vtable(&policy);
        let raw = Interface::as_raw(&policy);
        let hr0 = (vtbl.set_default_endpoint)(raw, pcw, 0);
        let _ = (vtbl.set_default_endpoint)(raw, pcw, 1);
        let _ = (vtbl.set_default_endpoint)(raw, pcw, 2);
        hr0.is_ok()
    }
}

unsafe fn device_id(dev: &IMMDevice) -> Option<String> {
    let pwstr = dev.GetId().ok()?;
    pwstr_to_string(pwstr.0)
}

unsafe fn device_name_and_kind(dev: &IMMDevice) -> (String, DeviceKind) {
    let Ok(store) = dev.OpenPropertyStore(STGM_READ) else {
        return (String::from("Audio"), DeviceKind::Generic);
    };
    let name = store
        .GetValue(&PKEY_Device_FriendlyName)
        .ok()
        .and_then(|pv| PropVariantToStringAlloc(&pv).ok())
        .and_then(|pwstr| pwstr_to_string(pwstr.0))
        .unwrap_or_else(|| String::from("Audio"));

    let form_factor = store
        .GetValue(&PKEY_ENDPOINT_FORMFACTOR)
        .ok()
        .and_then(|pv| PropVariantToUInt32(&pv).ok())
        .unwrap_or(0);

    let kind = DeviceKind::from_name_and_form(&name, form_factor);
    (name, kind)
}

unsafe fn pwstr_to_string(ptr: *mut u16) -> Option<String> {
    if ptr.is_null() {
        return None;
    }
    let mut len = 0usize;
    while *ptr.add(len) != 0 {
        len += 1;
    }
    let s = String::from_utf16_lossy(std::slice::from_raw_parts(ptr, len));
    windows::Win32::System::Com::CoTaskMemFree(Some(ptr as *const c_void));
    Some(s)
}

unsafe fn process_display_name(pid: u32) -> Option<(String, String, String)> {
    let handle = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid).ok()?;
    let mut buf = [0u16; 512];
    let mut size = buf.len() as u32;
    let ok = QueryFullProcessImageNameW(
        handle,
        PROCESS_NAME_WIN32,
        windows::core::PWSTR(buf.as_mut_ptr()),
        &mut size,
    )
    .is_ok();
    let _ = windows::Win32::Foundation::CloseHandle(handle);
    if !ok || size == 0 {
        return None;
    }
    let path = String::from_utf16_lossy(&buf[..size as usize]);
    let file_name = std::path::Path::new(&path)
        .file_stem()
        .and_then(|s| s.to_str())?;
    let stem_lower = file_name.to_ascii_lowercase();

    for (k, friendly) in KNOWN_COMM_APPS {
        if stem_lower == *k {
            return Some(((*friendly).to_string(), stem_lower, path));
        }
    }

    let friendly = match stem_lower.as_str() {
        "chrome" => "Google Chrome".to_string(),
        "msedge" => "Microsoft Edge".to_string(),
        "firefox" => "Firefox".to_string(),
        "brave" => "Brave".to_string(),
        "opera" => "Opera".to_string(),
        "spotify" => "Spotify".to_string(),
        "vlc" => "VLC".to_string(),
        "obs64" | "obs32" => "OBS Studio".to_string(),
        "steamwebhelper" | "steam" => "Steam".to_string(),
        _ => capitalize_first(file_name),
    };
    Some((friendly, stem_lower, path))
}

fn capitalize_first(s: &str) -> String {
    let mut chars = s.chars();
    match chars.next() {
        None => String::new(),
        Some(first) => first.to_uppercase().collect::<String>() + chars.as_str(),
    }
}

/// Zero-delay COM callback listener for volume/mute keys (`IAudioEndpointVolumeCallback`)
/// and default endpoint / device plug-in changes (`IMMNotificationClient`).
pub struct AudioEventWatcher {
    enumerator: IMMDeviceEnumerator,
    device_client: IMMNotificationClient,
    bound_endpoint_id: String,
    endpoint_vol: Option<IAudioEndpointVolume>,
    vol_client: IAudioEndpointVolumeCallback,
}

impl AudioEventWatcher {
    pub fn new(on_change: fn()) -> Option<Self> {
        unsafe {
            let enumerator: IMMDeviceEnumerator =
                CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_INPROC_SERVER).ok()?;
            let device_client = create_device_notify_client(on_change);
            let _ = enumerator.RegisterEndpointNotificationCallback(&device_client);
            let vol_client = create_volume_notify_client(on_change);

            let mut watcher = Self {
                enumerator,
                device_client,
                bound_endpoint_id: String::new(),
                endpoint_vol: None,
                vol_client,
            };
            watcher.rebind_default_endpoint();
            Some(watcher)
        }
    }

    pub fn snapshot(&self) -> Option<AudioSnapshot> {
        snapshot_with_enumerator(&self.enumerator)
    }

    /// Re-attaches `IAudioEndpointVolumeCallback` whenever the default playback device changes.
    pub fn rebind_default_endpoint(&mut self) {
        unsafe {
            let Ok(dev) = self.enumerator.GetDefaultAudioEndpoint(eRender, eConsole) else {
                return;
            };
            let id = device_id(&dev).unwrap_or_default();
            if !id.is_empty() && id == self.bound_endpoint_id && self.endpoint_vol.is_some() {
                return;
            }
            if let Some(old_vol) = self.endpoint_vol.take() {
                let _ = old_vol.UnregisterControlChangeNotify(&self.vol_client);
            }
            if let Ok(ep_vol) = dev.Activate::<IAudioEndpointVolume>(CLSCTX_ALL, None) {
                if ep_vol.RegisterControlChangeNotify(&self.vol_client).is_ok() {
                    self.bound_endpoint_id = id;
                    self.endpoint_vol = Some(ep_vol);
                }
            }
        }
    }
}

impl Drop for AudioEventWatcher {
    fn drop(&mut self) {
        unsafe {
            if let Some(old_vol) = self.endpoint_vol.take() {
                let _ = old_vol.UnregisterControlChangeNotify(&self.vol_client);
            }
            let _ = self
                .enumerator
                .UnregisterEndpointNotificationCallback(&self.device_client);
        }
    }
}

const IID_IUNKNOWN: GUID = GUID::from_u128(0x00000000_0000_0000_c000_000000000046);

#[repr(C)]
struct RawVolumeNotify {
    vtbl: *const IAudioEndpointVolumeCallback_Vtbl,
    ref_count: AtomicU32,
    on_change: fn(),
}

static VOLUME_NOTIFY_VTBL: IAudioEndpointVolumeCallback_Vtbl = IAudioEndpointVolumeCallback_Vtbl {
    base__: IUnknown_Vtbl {
        QueryInterface: vol_qi,
        AddRef: vol_add_ref,
        Release: vol_release,
    },
    OnNotify: vol_on_notify,
};

unsafe extern "system" fn vol_qi(
    this: *mut c_void,
    riid: *const GUID,
    ppv: *mut *mut c_void,
) -> HRESULT {
    if ppv.is_null() {
        return E_POINTER;
    }
    let Some(iid) = riid.as_ref() else {
        *ppv = std::ptr::null_mut();
        return E_POINTER;
    };
    if *iid == IID_IUNKNOWN || *iid == IAudioEndpointVolumeCallback::IID {
        *ppv = this;
        vol_add_ref(this);
        S_OK
    } else {
        *ppv = std::ptr::null_mut();
        E_NOINTERFACE
    }
}

unsafe extern "system" fn vol_add_ref(this: *mut c_void) -> u32 {
    let Some(obj) = (this as *const RawVolumeNotify).as_ref() else {
        return 0;
    };
    obj.ref_count.fetch_add(1, Ordering::Relaxed) + 1
}

unsafe extern "system" fn vol_release(this: *mut c_void) -> u32 {
    let Some(obj) = (this as *const RawVolumeNotify).as_ref() else {
        return 0;
    };
    let prev = obj.ref_count.fetch_sub(1, Ordering::Release);
    if prev == 1 {
        std::sync::atomic::fence(Ordering::Acquire);
        drop(Box::from_raw(this as *mut RawVolumeNotify));
    }
    prev - 1
}

unsafe extern "system" fn vol_on_notify(
    this: *mut c_void,
    _pnotify: *mut AUDIO_VOLUME_NOTIFICATION_DATA,
) -> HRESULT {
    if let Some(obj) = (this as *const RawVolumeNotify).as_ref() {
        (obj.on_change)();
    }
    S_OK
}

unsafe fn create_volume_notify_client(on_change: fn()) -> IAudioEndpointVolumeCallback {
    let raw = Box::into_raw(Box::new(RawVolumeNotify {
        vtbl: &VOLUME_NOTIFY_VTBL,
        ref_count: AtomicU32::new(1),
        on_change,
    }));
    IAudioEndpointVolumeCallback::from_raw(raw as *mut c_void)
}

#[repr(C)]
struct RawDeviceNotify {
    vtbl: *const IMMNotificationClient_Vtbl,
    ref_count: AtomicU32,
    on_change: fn(),
}

static DEVICE_NOTIFY_VTBL: IMMNotificationClient_Vtbl = IMMNotificationClient_Vtbl {
    base__: IUnknown_Vtbl {
        QueryInterface: dev_qi,
        AddRef: dev_add_ref,
        Release: dev_release,
    },
    OnDeviceStateChanged: dev_state_changed,
    OnDeviceAdded: dev_added,
    OnDeviceRemoved: dev_removed,
    OnDefaultDeviceChanged: dev_default_changed,
    OnPropertyValueChanged: dev_prop_changed,
};

unsafe extern "system" fn dev_qi(
    this: *mut c_void,
    riid: *const GUID,
    ppv: *mut *mut c_void,
) -> HRESULT {
    if ppv.is_null() {
        return E_POINTER;
    }
    let Some(iid) = riid.as_ref() else {
        *ppv = std::ptr::null_mut();
        return E_POINTER;
    };
    if *iid == IID_IUNKNOWN || *iid == IMMNotificationClient::IID {
        *ppv = this;
        dev_add_ref(this);
        S_OK
    } else {
        *ppv = std::ptr::null_mut();
        E_NOINTERFACE
    }
}

unsafe extern "system" fn dev_add_ref(this: *mut c_void) -> u32 {
    let Some(obj) = (this as *const RawDeviceNotify).as_ref() else {
        return 0;
    };
    obj.ref_count.fetch_add(1, Ordering::Relaxed) + 1
}

unsafe extern "system" fn dev_release(this: *mut c_void) -> u32 {
    let Some(obj) = (this as *const RawDeviceNotify).as_ref() else {
        return 0;
    };
    let prev = obj.ref_count.fetch_sub(1, Ordering::Release);
    if prev == 1 {
        std::sync::atomic::fence(Ordering::Acquire);
        drop(Box::from_raw(this as *mut RawDeviceNotify));
    }
    prev - 1
}

unsafe extern "system" fn dev_state_changed(
    this: *mut c_void,
    _pwstrdeviceid: PCWSTR,
    _dwnewstate: DEVICE_STATE,
) -> HRESULT {
    if let Some(obj) = (this as *const RawDeviceNotify).as_ref() {
        (obj.on_change)();
    }
    S_OK
}

unsafe extern "system" fn dev_added(this: *mut c_void, _pwstrdeviceid: PCWSTR) -> HRESULT {
    if let Some(obj) = (this as *const RawDeviceNotify).as_ref() {
        (obj.on_change)();
    }
    S_OK
}

unsafe extern "system" fn dev_removed(this: *mut c_void, _pwstrdeviceid: PCWSTR) -> HRESULT {
    if let Some(obj) = (this as *const RawDeviceNotify).as_ref() {
        (obj.on_change)();
    }
    S_OK
}

unsafe extern "system" fn dev_default_changed(
    this: *mut c_void,
    _flow: EDataFlow,
    _role: ERole,
    _pwstrdefaultdeviceid: PCWSTR,
) -> HRESULT {
    if let Some(obj) = (this as *const RawDeviceNotify).as_ref() {
        (obj.on_change)();
    }
    S_OK
}

unsafe extern "system" fn dev_prop_changed(
    _this: *mut c_void,
    _pwstrdeviceid: PCWSTR,
    _key: PROPERTYKEY,
) -> HRESULT {
    S_OK
}

unsafe fn create_device_notify_client(on_change: fn()) -> IMMNotificationClient {
    let raw = Box::into_raw(Box::new(RawDeviceNotify {
        vtbl: &DEVICE_NOTIFY_VTBL,
        ref_count: AtomicU32::new(1),
        on_change,
    }));
    IMMNotificationClient::from_raw(raw as *mut c_void)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classify_device_kinds() {
        assert_eq!(
            DeviceKind::from_name_and_form("Headset (PRO X Wireless Gaming Headset)", 1),
            DeviceKind::Headset
        );
        assert_eq!(
            DeviceKind::from_name_and_form("Fones de ouvido (AirPods Pro)", 1),
            DeviceKind::Headphones
        );
        assert_eq!(
            DeviceKind::from_name_and_form("LG UltraGear (NVIDIA High Definition Audio)", 1),
            DeviceKind::HdmiDisplay
        );
        assert_eq!(
            DeviceKind::from_name_and_form("Speakers (Realtek(R) Audio)", 1),
            DeviceKind::Speakers
        );
    }

    #[test]
    fn extract_short_device_label() {
        assert_eq!(
            DeviceKind::short_label("Speakers (Realtek(R) Audio)"),
            "Speakers"
        );
        assert_eq!(
            DeviceKind::short_label("Speakers (2- PRO X Wireless)"),
            "PRO X Wireless"
        );
        assert_eq!(
            DeviceKind::short_label("Alto-falantes (HyperX Cloud III Wireless)"),
            "HyperX Cloud III …"
        );
    }
}
