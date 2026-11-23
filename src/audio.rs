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
use windows::core::{GUID, HRESULT, PCWSTR};
use windows::Win32::Devices::FunctionDiscovery::PKEY_Device_FriendlyName;
use windows::Win32::Media::Audio::Endpoints::{IAudioEndpointVolume, IAudioMeterInformation};
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
pub struct AppSession {
    pub key: String,
    pub name: String,
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
        sessions: vec![
            AppSession {
                key: "discord".into(),
                name: "Discord".into(),
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

pub fn snapshot() -> Option<AudioSnapshot> {
    unsafe {
        let enumerator: IMMDeviceEnumerator =
            CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_INPROC_SERVER).ok()?;

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

        // 4. Per-app audio sessions on the default playback device
        let mut sessions = read_render_sessions(&default_dev);

        // 5. Check communication status:
        //    (a) Any active microphone capture session (eCapture)
        //    (b) Any active render session belonging to a known communication app (Teams/Discord/Zoom/etc.)
        //    (c) Differences between eConsole and eCommunications default session activity
        let mut comm_app_name = check_active_capture_sessions(&enumerator);
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

        let (proc_name, exe_stem) = if is_system_sounds || pid == 0 {
            (t().mixer_system_sounds.to_string(), String::from("system"))
        } else {
            match process_display_name(pid) {
                Some((_, stem)) if stem == "svchost" || stem == "audiodg" => {
                    (t().mixer_system_sounds.to_string(), String::from("system"))
                }
                Some((pretty, stem)) => (pretty, stem),
                None => {
                    // Process already exited; skip ghost session
                    if display_name.is_none() {
                        continue;
                    }
                    let dn = display_name.clone().unwrap_or_default();
                    (dn.clone(), dn.to_ascii_lowercase())
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
                if is_active {
                    existing.volume = volume;
                    existing.muted = muted;
                }
            })
            .or_insert(AppSession {
                key,
                name,
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
            if let Some((pretty, _)) = process_display_name(pid) {
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
            } else if let Some((_, stem)) = process_display_name(pid) {
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

/// Switches the default Windows audio output endpoint across Console, Multimedia, and Communications roles
/// using the undocumented `IPolicyConfig` COM interface (supported on Windows 7, 10, and 11).
pub fn set_default_output_device(endpoint_id: &str) -> bool {
    // CLSID_PolicyConfigClient: {870af99c-171d-4f9e-af0d-e63df40c2bc9}
    const CLSID_POLICY_CONFIG_CLIENT: GUID =
        GUID::from_u128(0x870af99c_171d_4f9e_af0d_e63df40c2bc9);
    // IID_IPolicyConfig (Windows 10/11): {f8679f50-850a-41cf-9c72-430f290290c8}
    const IID_IPOLICY_CONFIG_WIN10: GUID =
        GUID::from_u128(0xf8679f50_850a_41cf_9c72_430f290290c8);
    // IID_IPolicyConfigVista: {568b9108-44bf-40b4-9006-86afe5b5a620}
    const IID_IPOLICY_CONFIG_VISTA: GUID =
        GUID::from_u128(0x568b9108_44bf_40b4_9006_86afe5b5a620);

    #[repr(C)]
    struct IPolicyConfigVtbl {
        // IUnknown (3 methods)
        query_interface: unsafe extern "system" fn(*mut c_void, *const GUID, *mut *mut c_void) -> HRESULT,
        add_ref: unsafe extern "system" fn(*mut c_void) -> u32,
        release: unsafe extern "system" fn(*mut c_void) -> u32,
        // IPolicyConfig methods 0..9 before SetDefaultEndpoint (index 13 in vtable total)
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

    #[repr(C)]
    struct IPolicyConfigRaw {
        vtbl: *const IPolicyConfigVtbl,
    }

    unsafe {
        let mut raw_ptr: *mut c_void = std::ptr::null_mut();
        for iid in [IID_IPOLICY_CONFIG_WIN10, IID_IPOLICY_CONFIG_VISTA] {
            if co_create_raw(&CLSID_POLICY_CONFIG_CLIENT, &iid, &mut raw_ptr) && !raw_ptr.is_null()
            {
                break;
            }
        }
        if raw_ptr.is_null() {
            return false;
        }

        let obj = raw_ptr as *mut IPolicyConfigRaw;
        let id_w = wide(endpoint_id);
        let pcw = PCWSTR(id_w.as_ptr());

        // eConsole = 0, eMultimedia = 1, eCommunications = 2
        let hr0 = ((*(*obj).vtbl).set_default_endpoint)(raw_ptr, pcw, 0);
        let _ = ((*(*obj).vtbl).set_default_endpoint)(raw_ptr, pcw, 1);
        let _ = ((*(*obj).vtbl).set_default_endpoint)(raw_ptr, pcw, 2);
        ((*(*obj).vtbl).release)(raw_ptr);

        hr0.is_ok()
    }
}

unsafe fn co_create_raw(clsid: *const GUID, iid: *const GUID, out: *mut *mut c_void) -> bool {
    #[link(name = "ole32")]
    extern "system" {
        fn CoCreateInstance(
            rclsid: *const GUID,
            pUnkOuter: *mut c_void,
            dwClsContext: u32,
            riid: *const GUID,
            ppv: *mut *mut c_void,
        ) -> HRESULT;
    }
    CoCreateInstance(clsid, std::ptr::null_mut(), CLSCTX_ALL.0, iid, out).is_ok()
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

unsafe fn process_display_name(pid: u32) -> Option<(String, String)> {
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
            return Some(((*friendly).to_string(), stem_lower));
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
    Some((friendly, stem_lower))
}

fn capitalize_first(s: &str) -> String {
    let mut chars = s.chars();
    match chars.next() {
        None => String::new(),
        Some(first) => first.to_uppercase().collect::<String>() + chars.as_str(),
    }
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
