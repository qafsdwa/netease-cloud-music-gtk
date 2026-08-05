//
// global_shortcut.rs
// Copyright (C) 2026
// Distributed under terms of the GPL-3.0-or-later license.
//
// 通过 XDG Desktop Portal (org.freedesktop.portal.GlobalShortcuts) 实现全局快捷键。
// 支持 KDE Plasma (X11/Wayland) 等实现了该 portal 的桌面环境，快捷键在系统设置中配置。
// 在未实现该 portal 的环境（如 GNOME）中静默降级，不影响应用正常启动。
//

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::thread;

use async_channel::Sender;
use gettextrs::gettext;
use log::*;
use zbus::blocking::{Connection, MessageIterator};
use zbus::zvariant::{ObjectPath, OwnedValue, Str, Value};
use zbus::Message;

use crate::application::Action;

const PORTAL_DEST: &str = "org.freedesktop.portal.Desktop";
const PORTAL_PATH: &str = "/org/freedesktop/portal/desktop";
const PORTAL_IFACE: &str = "org.freedesktop.portal.GlobalShortcuts";
const REQUEST_IFACE: &str = "org.freedesktop.portal.Request";
const SESSION_IFACE: &str = "org.freedesktop.portal.Session";

// (id, description, preferred_trigger)
// KDE 的 xdg-desktop-portal-kde 只接受大写修饰键（CTRL/ALT/SHIFT/LOGO/NUM/CAPS）+ XKB 键名
const SHORTCUTS: &[(&str, &str, &str)] = &[
    ("play-pause", "Play/Pause", "CTRL+ALT+p"),
    ("next-song", "Next song", "CTRL+ALT+Right"),
    ("prev-song", "Previous song", "CTRL+ALT+Left"),
    ("volume-up", "Volume up", "CTRL+ALT+Up"),
    ("volume-down", "Volume down", "CTRL+ALT+Down"),
];

fn shortcut_to_action(id: &str) -> Option<Action> {
    match id {
        "play-pause" => Some(Action::TogglePlayPause),
        "next-song" => Some(Action::PlayNextSong),
        "prev-song" => Some(Action::PlayPreviousSong),
        "volume-up" => Some(Action::VolumeUp),
        "volume-down" => Some(Action::VolumeDown),
        _ => None,
    }
}

// 组件名必须避开 kglobalacceld 的两条死路（否则注册被静默丢弃）：
// - 以 ".desktop" 结尾会被当作 KServiceActionComponent，只加载桌面文件声明的动作；
//   找不到桌面文件时组件创建直接返回 nullptr，doRegister/setShortcut 全部静默失效。
// - 与已有组件重名（如终端启动时 portal-kde 会把 app_id 透传成组件名，
//   此时动作被挂到无关组件的 context 上，同样静默失效）。
// 因此显式传入一个稳定的自定义 app_id，portal-kde 会把它用作组件名。
const APP_ID: &str = "com.gitee.gmg137.NeteaseCloudMusicGtk4";

fn options_dict(handle_token: &str) -> HashMap<String, Value<'static>> {
    HashMap::from([
        ("app_id".to_string(), Value::from(APP_ID.to_string())),
        (
            "handle_token".to_string(),
            Value::from(handle_token.to_string()),
        ),
        // xdg-desktop-portal >= 1.22 将 session 的 token 键改名为 session_handle_token
        (
            "session_handle_token".to_string(),
            Value::from(handle_token.to_string()),
        ),
    ])
}

fn build_shortcuts() -> Vec<(String, HashMap<String, Value<'static>>)> {
    SHORTCUTS
        .iter()
        .map(|(id, desc, trigger)| {
            let props = HashMap::from([
                ("description".to_string(), Value::from(gettext(*desc))),
                ("preferred_trigger".to_string(), Value::from(*trigger)),
            ]);
            (id.to_string(), props)
        })
        .collect()
}

fn response_code(msg: &Message) -> Option<(u32, HashMap<String, OwnedValue>)> {
    let body = match msg
        .body()
        .deserialize::<(u32, HashMap<String, OwnedValue>)>()
    {
        Ok(body) => body,
        Err(e) => {
            debug!("解析 Request::Response 失败: {}", e);
            return None;
        }
    };
    Some(body)
}

pub struct GlobalShortcutHandle {
    conn: Option<Connection>,
    session_handle: Arc<Mutex<Option<ObjectPath<'static>>>>,
}

impl GlobalShortcutHandle {
    pub fn new() -> Self {
        Self {
            conn: None,
            session_handle: Arc::new(Mutex::new(None)),
        }
    }

    pub fn start(&mut self, sender: Sender<Action>) {
        if self.conn.is_some() {
            info!("全局快捷键已在运行");
            return;
        }

        let conn = match Connection::session() {
            Ok(conn) => conn,
            Err(e) => {
                warn!("无法连接 D-Bus 会话总线: {}", e);
                return;
            }
        };
        info!("启动全局快捷键");

        let conn_clone = conn.clone();
        let session_handle = self.session_handle.clone();
        thread::spawn(move || {
            if let Err(e) = global_shortcut_loop(conn_clone, sender, session_handle) {
                warn!("全局快捷键初始化失败: {}", e);
            }
        });

        self.conn = Some(conn);
    }

    pub fn stop(&mut self) {
        if let Some(conn) = self.conn.take() {
            if let Some(session) = self.session_handle.lock().unwrap().take() {
                let _ = conn.call_method(
                    Some(PORTAL_DEST),
                    session,
                    Some(SESSION_IFACE),
                    "Close",
                    &(),
                );
            }
            info!("已停止全局快捷键");
        }
    }

    pub fn is_running(&self) -> bool {
        self.conn.is_some()
            && self
                .session_handle
                .lock()
                .map(|s| s.is_some())
                .unwrap_or(false)
    }

    // 打开系统设置中的全局快捷键配置界面，返回是否成功发出请求
    pub fn configure(&self) -> bool {
        let Some(conn) = &self.conn else {
            return false;
        };
        let session = self.session_handle.lock().unwrap().clone();
        let Some(session) = session else {
            return false;
        };
        let options: HashMap<String, Value<'static>> = HashMap::new();
        match conn.call_method(
            Some(PORTAL_DEST),
            PORTAL_PATH,
            Some(PORTAL_IFACE),
            "ConfigureShortcuts",
            &(session, "", options),
        ) {
            Ok(_) => true,
            Err(e) => {
                warn!("打开全局快捷键配置界面失败: {}", e);
                false
            }
        }
    }
}

impl Default for GlobalShortcutHandle {
    fn default() -> Self {
        Self::new()
    }
}

impl Drop for GlobalShortcutHandle {
    fn drop(&mut self) {
        self.stop();
    }
}

fn global_shortcut_loop(
    conn: Connection,
    sender: Sender<Action>,
    shared_session: Arc<Mutex<Option<ObjectPath<'static>>>>,
) -> zbus::Result<()> {
    // 1. 创建全局快捷键会话
    let handle_token = format!("ncm_shortcuts_{}", fastrand::u32(..));
    let reply = conn.call_method(
        Some(PORTAL_DEST),
        PORTAL_PATH,
        Some(PORTAL_IFACE),
        "CreateSession",
        &(options_dict(&handle_token),),
    )?;
    let _request_handle = reply.body().deserialize::<ObjectPath<'_>>()?;

    // 2. 监听 Request::Response 与 Activated 信号
    let mut iter = MessageIterator::from(&conn);
    let mut session_handle: Option<ObjectPath<'static>> = None;

    while let Some(item) = iter.next() {
        let msg = match item {
            Ok(msg) => msg,
            Err(e) => {
                debug!("D-Bus 消息错误: {}", e);
                continue;
            }
        };

        let header = msg.header();
        let Some(interface) = header.interface().map(|i| i.as_str().to_string()) else {
            continue;
        };
        let Some(member) = header.member().map(|m| m.as_str().to_string()) else {
            continue;
        };
        let _path = header.path().cloned();

        if interface == REQUEST_IFACE && member == "Response" {
            let Some((code, results)) = response_code(&msg) else {
                continue;
            };
            if code != 0 {
                let error = results
                    .get("error")
                    .and_then(|v| v.downcast_ref::<Str>().ok())
                    .map(|s| s.to_string())
                    .unwrap_or_default();
                warn!("全局快捷键请求被拒绝: {} {}", code, error);
                return Ok(());
            }
            if session_handle.is_none() {
                // CreateSession 的响应，从中获取 session_handle
                let Some(session) = results
                    .get("session_handle")
                    .and_then(|v| v.downcast_ref::<Str>().ok())
                    .map(|s| s.to_string())
                    .and_then(|s| ObjectPath::try_from(s).ok())
                else {
                    warn!("创建全局快捷键会话失败: 缺少 session_handle");
                    return Ok(());
                };
                session_handle = Some(session.clone());
                if let Ok(mut shared) = shared_session.lock() {
                    *shared = Some(session.clone());
                }
                info!("全局快捷键会话创建成功");

                // 3. 绑定快捷键
                let bind_token = format!("ncm_shortcuts_bind_{}", fastrand::u32(..));
                let reply = conn.call_method(
                    Some(PORTAL_DEST),
                    PORTAL_PATH,
                    Some(PORTAL_IFACE),
                    "BindShortcuts",
                    &(session, build_shortcuts(), "", options_dict(&bind_token)),
                )?;
                let _ = reply.body().deserialize::<ObjectPath<'_>>()?;
            } else {
                // BindShortcuts 的响应
                info!("全局快捷键注册成功");
            }
        } else if interface == PORTAL_IFACE && member == "Activated" && session_handle.is_some() {
            match msg.body().deserialize::<(
                ObjectPath<'_>,
                String,
                u64,
                HashMap<String, OwnedValue>,
            )>() {
                Ok((sig_session, shortcut_id, _, _)) => {
                    if sig_session.as_str() != session_handle.as_ref().unwrap().as_str() {
                        continue;
                    }
                    if let Some(action) = shortcut_to_action(&shortcut_id) {
                        if sender.try_send(action).is_err() {
                            debug!("全局快捷键发送 Action 失败");
                        }
                    }
                }
                Err(e) => debug!("解析 Activated 信号失败: {}", e),
            }
        }
    }
    Ok(())
}
