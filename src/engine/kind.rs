//! 文件类型分类：界面徽标与“按类型分类保存”共用同一份映射。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Category {
    Archive,
    Video,
    Audio,
    Document,
    Program,
    DiskImage,
    Image,
    Other,
}

pub fn ext_of(name: &str) -> String {
    name.rsplit_once('.').map(|(_, e)| e.to_ascii_lowercase()).unwrap_or_default()
}

pub fn category(name: &str) -> Category {
    match ext_of(name).as_str() {
        "zip" | "rar" | "7z" | "gz" | "xz" | "bz2" | "tar" | "zst" | "tgz" | "cab" => Category::Archive,
        "mp4" | "mkv" | "avi" | "mov" | "wmv" | "flv" | "webm" | "m4v" | "ts" | "rmvb" => Category::Video,
        "mp3" | "flac" | "wav" | "m4a" | "aac" | "ogg" | "opus" | "ape" | "wma" => Category::Audio,
        "pdf" | "doc" | "docx" | "xls" | "xlsx" | "ppt" | "pptx" | "txt" | "md" | "epub" | "csv" | "mobi" => {
            Category::Document
        }
        "exe" | "msi" | "msix" | "apk" | "dmg" | "deb" | "rpm" | "appimage" | "bat" | "appx" => Category::Program,
        "iso" | "img" | "vhd" | "vhdx" | "vmdk" | "wim" => Category::DiskImage,
        "jpg" | "jpeg" | "png" | "gif" | "webp" | "bmp" | "svg" | "heic" | "tif" | "tiff" => Category::Image,
        _ => Category::Other,
    }
}

impl Category {
    /// 自动分类保存时使用的子目录名
    pub fn folder(self) -> &'static str {
        match self {
            Self::Archive => "压缩文件",
            Self::Video => "视频",
            Self::Audio => "音乐",
            Self::Document => "文档",
            Self::Program => "程序",
            Self::DiskImage => "镜像文件",
            Self::Image => "图片",
            Self::Other => "其他",
        }
    }
}
