use std::{env, fs, io::Read, path::PathBuf, ptr, slice};

use anyhow::{Context, Result};
use pdfium_render::prelude::*;
use windows::{
    Globalization::Language,
    Graphics::Imaging::{BitmapBufferAccessMode, BitmapPixelFormat, SoftwareBitmap},
    Media::Ocr::OcrEngine,
    Win32::System::WinRT::IMemoryBufferByteAccess,
    core::{HSTRING, Interface},
};

/// 対象の PDF に頻出する文字列の辞書（調整要）
const DICTIONARY: &[&str] = &[
    "検査", "成績", "寸法", "製造", "合格", "工場", "図面", "番号", "日付", "規格", "証明", "会社",
    "引張", "試験", "位置", "材質", "公差", "外観", "inspec", "resul", "dimens", "job", "accep",
    "works", "date", "speci", "certi", "compa", "tensil", "test", "posit", "mater", "visua",
];
/// 辞書に登録されている文字列が見つかったときに使用するしきい値（調整要）
const THRESHOLD: usize = 8;

/// 指定された PdfPageRenderRotation で PdfPage をレンダリングし、画像の幅、高さ、ビットマップバッファを返す
fn render(page: &PdfPage, rotation: PdfPageRenderRotation) -> Result<(u32, u32, Vec<u8>)> {
    let config = PdfRenderConfig::new()
        .rotate(rotation, true)
        .use_grayscale_rendering(true)
        .set_image_smoothing(false)
        .set_target_width(1920)
        .set_maximum_height(1920);
    let img = page.render_with_config(&config)?.as_image()?;
    let width = img.width();
    let height = img.height();
    let buf = img.to_rgba8().to_vec();
    Ok((width, height, buf))
}

/// レンダリングされた画像の幅、高さ、バッファを受け取って、OCR した結果の文字列を返す
fn ocr(width: u32, height: u32, buf: Vec<u8>, tag: &HSTRING) -> Result<String> {
    let bmp = SoftwareBitmap::Create(BitmapPixelFormat::Bgra8, width as i32, height as i32)?;
    {
        let bmp_buf = bmp.LockBuffer(BitmapBufferAccessMode::Write)?;
        let array: IMemoryBufferByteAccess = bmp_buf.CreateReference()?.cast()?;

        let mut data = ptr::null_mut();
        let mut capacity = 0;
        unsafe { array.GetBuffer(&mut data, &mut capacity)? };

        assert_eq!(width * height * 4, capacity);

        let slice = unsafe { slice::from_raw_parts_mut(data, capacity as usize) };
        slice.clone_from_slice(&buf);
    }
    let lang = Language::CreateLanguage(tag)?;
    let engine = OcrEngine::TryCreateFromLanguage(&lang)?;
    let result = engine
        .RecognizeAsync(&bmp)?
        .join()?
        .Text()?
        .to_string_lossy()
        .chars()
        .filter(|s| s != &' ')
        .collect::<String>()
        .to_lowercase();

    Ok(result)
}

/// PdfPage を 90 degrees ずつ回転させて、認識された文字数が最も多かった PdfPageRenderRotation を返す
fn detect(page: &PdfPage, tag: &HSTRING) -> Result<PdfPageRenderRotation> {
    let rotation = [
        PdfPageRenderRotation::None,
        PdfPageRenderRotation::Degrees90,
        PdfPageRenderRotation::Degrees180,
        PdfPageRenderRotation::Degrees270,
    ]
    .into_iter()
    .filter_map(|r| {
        let (width, height, buf) = render(page, r).ok()?;
        let ocr_str = ocr(width, height, buf, tag).ok()?;
        // dbg!(&ocr_str);
        let hit = DICTIONARY.iter().filter(|s| ocr_str.contains(*s)).count();
        // 辞書に登録されている文字が見つかった場合は結果に係数を掛けて優先度を上げる
        let result = if hit > 0 {
            ocr_str.len() * (1 + 1 / THRESHOLD * hit)
        } else {
            ocr_str.len()
        };
        Some((r, result))
    })
    .max_by(|(_, a), (_, b)| a.cmp(b))
    .unwrap_or((PdfPageRenderRotation::None, 0))
    .0;
    Ok(rotation)
}

fn start() -> Result<()> {
    let arg = env::args()
        .nth(1)
        .map(PathBuf::from)
        .context("PDF ファイルが見つかりません。")?;

    let lang_tags = OcrEngine::AvailableRecognizerLanguages()?
        .First()?
        .filter_map(|l| Some((l.DisplayName().ok()?, l.LanguageTag().ok()?)))
        .collect::<Vec<_>>();

    let tag = if lang_tags.len() == 1 {
        &lang_tags[0].1
    } else if lang_tags.len() > 1 {
        println!("対象の言語を選択してください。");
        for (i, (name, _)) in lang_tags.iter().enumerate() {
            println!("{}: {}", i + 1, name);
        }
        let mut buf = String::new();
        std::io::stdin().read_line(&mut buf)?;
        &lang_tags[buf.trim().parse::<usize>()? - 1].1
    } else {
        panic!("No availavle recognizer language");
    };

    let pdf = fs::read(&arg)?;
    let pdfium = Pdfium::default();
    let src_doc = pdfium.load_pdf_from_byte_vec(pdf, None)?;
    let mut dst_doc = pdfium.create_new_pdf()?;
    for (i, page) in src_doc.pages().iter().enumerate() {
        println!("[{:>3}/{:>3}] ページを処理中", i + 1, src_doc.pages().len());
        let rotation = detect(&page, tag)?;
        // dbg!(page.rotation()?);
        // dbg!(rotation);
        let len = dst_doc.pages().len();
        dst_doc
            .pages_mut()
            .copy_page_from_document(&src_doc, i as i32, len)?;
        use PdfPageRenderRotation::*;
        let rotation = match (rotation, page.rotation()?) {
            (None, r) => r,
            (r, None) => r,
            (Degrees90, Degrees90) => Degrees180,
            (Degrees180, Degrees90) => Degrees270,
            (Degrees270, Degrees90) => None,
            (Degrees90, Degrees180) => Degrees270,
            (Degrees180, Degrees180) => None,
            (Degrees270, Degrees180) => Degrees90,
            (Degrees90, Degrees270) => None,
            (Degrees180, Degrees270) => Degrees90,
            (Degrees270, Degrees270) => Degrees180,
        };
        dst_doc.pages_mut().last()?.set_rotation(rotation);
    }

    let parent = arg.parent().context("フォルダが見つかりません。")?;
    let file_stem = arg.file_stem().context("ファイル名が見つかりません。")?;
    let file_stem = file_stem.to_string_lossy();
    let target = parent.join(format!("{file_stem}_.pdf"));

    dst_doc.save_to_file(&target)?;

    Ok(())
}

fn main() {
    match start() {
        Ok(_) => println!("正常に終了しました。"),
        Err(e) => println!("エラーが発生しました： {e}"),
    }
    println!("終了するにはエンターキーを押してください。");
    std::io::stdin().read_exact(&mut [0]).unwrap();
}
