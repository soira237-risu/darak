//! 확장자 → 범주 (treemap 색상과 통계용)

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Category {
    Other,
    Video,
    Audio,
    Image,
    Document,
    Archive,
    Code,
    Executable,
    System,
    Data,
}

impl Category {
    pub const ALL: [Category; 10] = [
        Category::Video,
        Category::Audio,
        Category::Image,
        Category::Document,
        Category::Archive,
        Category::Code,
        Category::Executable,
        Category::System,
        Category::Data,
        Category::Other,
    ];

    pub fn label(self) -> &'static str {
        match self {
            Category::Other => "기타",
            Category::Video => "동영상",
            Category::Audio => "오디오",
            Category::Image => "이미지",
            Category::Document => "문서",
            Category::Archive => "압축",
            Category::Code => "코드",
            Category::Executable => "실행 파일",
            Category::System => "시스템",
            Category::Data => "데이터",
        }
    }
}

pub fn categorize(name: &str) -> Category {
    let Some(dot) = name.rfind('.') else { return Category::Other };
    let ext = &name[dot + 1..];
    if ext.len() > 8 {
        return Category::Other;
    }
    let mut buf = [0u8; 8];
    let lower = &mut buf[..ext.len()];
    lower.copy_from_slice(ext.as_bytes());
    lower.make_ascii_lowercase();
    match &*lower {
        b"mp4" | b"mkv" | b"avi" | b"mov" | b"wmv" | b"webm" | b"m4v" | b"flv" | b"ts" | b"m2ts" | b"mpg" | b"mpeg" => {
            Category::Video
        }
        b"mp3" | b"flac" | b"wav" | b"aac" | b"ogg" | b"m4a" | b"wma" | b"opus" => Category::Audio,
        b"jpg" | b"jpeg" | b"png" | b"gif" | b"bmp" | b"webp" | b"tif" | b"tiff" | b"heic" | b"psd" | b"raw"
        | b"cr2" | b"nef" | b"svg" | b"ico" => Category::Image,
        b"pdf" | b"doc" | b"docx" | b"xls" | b"xlsx" | b"ppt" | b"pptx" | b"hwp" | b"hwpx" | b"txt" | b"md"
        | b"rtf" | b"odt" | b"epub" => Category::Document,
        b"zip" | b"7z" | b"rar" | b"gz" | b"tar" | b"xz" | b"zst" | b"bz2" | b"iso" | b"cab" | b"wim" | b"esd" => {
            Category::Archive
        }
        b"rs" | b"c" | b"h" | b"cpp" | b"hpp" | b"cs" | b"js" | b"tsx" | b"jsx" | b"py" | b"go" | b"java"
        | b"kt" | b"json" | b"toml" | b"yaml" | b"yml" | b"xml" | b"html" | b"css" | b"rlib" | b"o" | b"obj"
        | b"pdb" | b"lib" | b"a" => Category::Code,
        b"exe" | b"dll" | b"msi" | b"msix" | b"appx" | b"sys" | b"com" | b"bat" | b"ps1" | b"jar" => {
            Category::Executable
        }
        b"etl" | b"log" | b"evtx" | b"dmp" | b"tmp" | b"cat" | b"mui" | b"manifest" | b"pf" => Category::System,
        b"db" | b"sqlite" | b"vhd" | b"vhdx" | b"vmdk" | b"pak" | b"bin" | b"dat" | b"bak" | b"mdb" | b"ldf"
        | b"mdf" | b"pst" | b"ost" => Category::Data,
        _ => Category::Other,
    }
}
