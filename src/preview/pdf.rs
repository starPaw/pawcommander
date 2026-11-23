mod protocol;
mod view;
mod worker;

use crate::file_error::{PdfError, PdfProblem as Problem};
use serde::{Deserialize, Serialize};

pub(super) use view::Document;
pub(crate) use worker::run as run_pdf_worker;
pub(crate) const PDF_WORKER_ARG: &str = "--paw-pdf-preview-worker";
pub(super) const FILE_LIMIT: usize = 64 * 1024 * 1024;
const PAGE_LIMIT: usize = 10_000;
const MAX_EDGE: u32 = 3072;
const QUERY_LIMIT: usize = 128;

#[derive(Serialize, Deserialize)]
enum Command {
    Render {
        page: usize,
        edge: u32,
        highlight: String,
    },
    Search {
        query: String,
    },
}

#[derive(Serialize, Deserialize)]
enum Reply {
    Opened {
        pages: usize,
    },
    Image {
        width: u32,
        height: u32,
        points: [f64; 2],
    },
    Matches(Vec<usize>),
    Error(Problem),
}

struct Image {
    width: u32,
    height: u32,
    points: [f64; 2],
    rgba: Vec<u8>,
}

enum Answer {
    Page(Image),
    Matches(Vec<usize>),
    Error(PdfError),
}

fn accepts_query(text: &str) -> bool {
    text.chars().count() <= QUERY_LIMIT
}

fn image_bytes(width: u32, height: u32) -> Option<usize> {
    if width == 0 || height == 0 || width > MAX_EDGE || height > MAX_EDGE {
        return None;
    }
    Some(width as usize * height as usize * 4)
}

pub(super) fn is_pdf(path: &std::path::Path) -> bool {
    path.extension()
        .is_some_and(|extension| extension.eq_ignore_ascii_case("pdf"))
}
