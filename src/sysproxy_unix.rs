// 非 Windows：系统代理功能不支持（页面开关随 supported:false 隐藏）。

use std::path::Path;

#[derive(Clone, Debug)]
pub struct Status {
    pub enabled: bool,
    pub server: String,
}

pub fn status(_data_dir: &Path) -> Status {
    Status {
        enabled: false,
        server: String::new(),
    }
}

pub fn set_enabled(_on: bool, _mixed_port: u16, _data_dir: &Path) -> Result<(), String> {
    Err("系统代理仅 Windows 支持".into())
}
