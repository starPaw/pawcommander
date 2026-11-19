use super::{
    accepts_query, image_bytes, Command, Image, Problem, Reply, FILE_LIMIT, MAX_EDGE, PAGE_LIMIT,
};
use crate::preview::helper::{read_message, read_size, write_message};
use std::{
    ffi::c_void,
    io::{self, BufReader, BufWriter, Read, Write},
    ptr::NonNull,
};

unsafe extern "C" {
    fn paw_pdf_open(
        bytes: *const u8,
        length: usize,
        pages: *mut usize,
        locked: *mut bool,
    ) -> *mut c_void;
    fn paw_pdf_close(handle: *mut c_void);
    fn paw_pdf_contains(handle: *mut c_void, index: usize, query: *const u8, length: usize) -> i32;
    fn paw_pdf_render(
        handle: *mut c_void,
        index: usize,
        edge: u32,
        highlight: *const u8,
        highlight_len: usize,
        pixels: *mut u8,
        capacity: usize,
        width: *mut u32,
        height: *mut u32,
        point_width: *mut f64,
        point_height: *mut f64,
    ) -> bool;
}

pub(crate) fn run() -> i32 {
    let mut input = BufReader::new(io::stdin().lock());
    let mut output = BufWriter::new(io::stdout().lock());
    i32::from(serve(&mut input, &mut output).is_err())
}

fn serve(input: &mut impl Read, output: &mut impl Write) -> io::Result<()> {
    let size = read_size(input)?;
    if size == 0 || size > FILE_LIMIT {
        let reason = if size > FILE_LIMIT {
            Problem::TooLarge
        } else {
            Problem::Invalid
        };
        write_message(output, &Reply::Error(reason))?;
        return output.flush();
    }
    let mut bytes = vec![0; size];
    input.read_exact(&mut bytes)?;
    let document = Pdf::open(&bytes);
    drop(bytes);
    let document = match document {
        Ok(document) => document,
        Err(reason) => {
            write_message(output, &Reply::Error(reason))?;
            return output.flush();
        }
    };
    write_message(
        output,
        &Reply::Opened {
            pages: document.pages,
        },
    )?;
    output.flush()?;
    loop {
        let command: Command = match read_message(input) {
            Ok(command) => command,
            Err(error) if error.kind() == io::ErrorKind::UnexpectedEof => return Ok(()),
            Err(error) => return Err(error),
        };
        match command {
            Command::Render {
                page,
                edge,
                highlight,
            } => match document.render(page, edge, &highlight) {
                Ok(Image {
                    width,
                    height,
                    points,
                    rgba,
                }) => {
                    write_message(
                        output,
                        &Reply::Image {
                            width,
                            height,
                            points,
                        },
                    )?;
                    output.write_all(&rgba)?;
                }
                Err(reason) => write_message(output, &Reply::Error(reason))?,
            },
            Command::Search { query } => {
                let reply = match document.search(&query) {
                    Ok(matches) => Reply::Matches(matches),
                    Err(reason) => Reply::Error(reason),
                };
                write_message(output, &reply)?;
            }
        }
        output.flush()?;
    }
}

struct Pdf {
    handle: NonNull<c_void>,
    pages: usize,
}

impl Pdf {
    fn open(bytes: &[u8]) -> Result<Self, Problem> {
        let mut locked = false;
        let mut pages = 0;
        // The bridge copies the supplied bytes and retains its document.
        let handle = unsafe { paw_pdf_open(bytes.as_ptr(), bytes.len(), &mut pages, &mut locked) };
        let document = Self {
            handle: NonNull::new(handle).ok_or(Problem::Invalid)?,
            pages,
        };
        if locked {
            return Err(Problem::Locked);
        }
        if pages == 0 {
            return Err(Problem::Invalid);
        }
        if pages > PAGE_LIMIT {
            return Err(Problem::TooManyPages);
        }
        Ok(document)
    }

    fn render(&self, page: usize, edge: u32, highlight: &str) -> Result<Image, Problem> {
        if page >= self.pages || !(1..=MAX_EDGE).contains(&edge) || !accepts_query(highlight) {
            return Err(Problem::Page);
        }
        let mut rgba = vec![0; image_bytes(edge, edge).ok_or(Problem::Page)?];
        let (mut width, mut height) = (0, 0);
        let [mut point_width, mut point_height] = [0.0; 2];
        // The document is used on this helper thread. The native call writes
        // within the supplied buffer and does not store its address.
        let rendered = unsafe {
            paw_pdf_render(
                self.handle.as_ptr(),
                page,
                edge,
                highlight.as_ptr(),
                highlight.len(),
                rgba.as_mut_ptr(),
                rgba.len(),
                &mut width,
                &mut height,
                &mut point_width,
                &mut point_height,
            )
        };
        let len = image_bytes(width, height)
            .filter(|&len| rendered && len <= rgba.len())
            .ok_or(Problem::Page)?;
        rgba.truncate(len);
        Ok(Image {
            width,
            height,
            points: [point_width, point_height],
            rgba,
        })
    }

    fn search(&self, query: &str) -> Result<Vec<usize>, Problem> {
        if query.is_empty() || !accepts_query(query) {
            return Err(Problem::Search);
        }
        let mut matches = Vec::new();
        for page in 0..self.pages {
            // The bridge reads this UTF-8 query only for the duration of the call.
            match unsafe {
                paw_pdf_contains(self.handle.as_ptr(), page, query.as_ptr(), query.len())
            } {
                1 => matches.push(page),
                0 => {}
                _ => return Err(Problem::Search),
            }
        }
        Ok(matches)
    }
}

impl Drop for Pdf {
    fn drop(&mut self) {
        // This balances the bridge's retained document.
        unsafe { paw_pdf_close(self.handle.as_ptr()) };
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::preview::helper::write_blob;
    use std::io::Cursor;

    #[test]
    fn pdf_worker_handles_corruption_and_renders_rotated_search_results() {
        let mut input = Vec::new();
        write_blob(&mut input, b"%PDF-1.4\nNo page objects\n%%EOF\n").unwrap();
        let mut output = Vec::new();
        serve(&mut Cursor::new(input), &mut output).unwrap();
        assert!(matches!(
            read_message(&mut Cursor::new(output)),
            Ok(Reply::Error(Problem::Invalid))
        ));

        let mut input = Vec::new();
        write_blob(&mut input, &sample_pdf()).unwrap();
        write_message(
            &mut input,
            &Command::Search {
                query: "fInD Me".into(),
            },
        )
        .unwrap();
        write_message(
            &mut input,
            &Command::Render {
                page: 2,
                edge: 200,
                highlight: String::new(),
            },
        )
        .unwrap();
        write_message(
            &mut input,
            &Command::Render {
                page: 1,
                edge: 200,
                highlight: "find me".into(),
            },
        )
        .unwrap();
        write_message(
            &mut input,
            &Command::Render {
                page: 0,
                edge: 200,
                highlight: String::new(),
            },
        )
        .unwrap();
        let mut output = Vec::new();
        serve(&mut Cursor::new(input), &mut output).unwrap();
        let mut output = Cursor::new(output);
        assert!(matches!(
            read_message(&mut output),
            Ok(Reply::Opened { pages: 2 })
        ));
        assert!(matches!(read_message(&mut output), Ok(Reply::Matches(indices)) if indices == [1]));
        assert!(matches!(
            read_message(&mut output),
            Ok(Reply::Error(Problem::Page))
        ));
        assert!(matches!(
            read_message(&mut output),
            Ok(Reply::Image {
                width: 100,
                height: 200,
                points: [100.0, 200.0],
            })
        ));
        let mut pixels = vec![0; 100 * 200 * 4];
        output.read_exact(&mut pixels).unwrap();
        assert!(
            pixels
                .as_chunks::<4>()
                .0
                .iter()
                .any(|p| p[0] > 220 && p[1] > 170 && p[2] < 215 && p[3] == 255),
            "expected the search highlight"
        );
        assert!(matches!(
            read_message(&mut output),
            Ok(Reply::Image {
                width: 200,
                height: 200,
                points: [100.0, 100.0],
            })
        ));
        pixels.resize(200 * 200 * 4, 0);
        output.read_exact(&mut pixels).unwrap();
        let blue = pixels
            .as_chunks::<4>()
            .0
            .iter()
            .filter(|&&p| p == [0, 0, 255, 255])
            .count();
        assert_eq!(
            blue,
            50 * 28,
            "the 25 × 14 point shape must scale along with the page"
        );
    }

    fn sample_pdf() -> Vec<u8> {
        let content = "BT /F1 13 Tf 14 40 Td (Find me here) Tj ET\n";
        let blue = "0 0 1 rg 10 70 25 14 re f\n";
        let objects = [
            "<< /Type /Catalog /Pages 2 0 R >>".into(),
            "<< /Type /Pages /Kids [3 0 R 4 0 R] /Count 2 >>".into(),
            "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 100 100] /Contents 7 0 R >>".into(),
            "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 200 100] /Rotate 90 /Resources << /Font << /F1 5 0 R >> >> /Contents 6 0 R >>".into(),
            "<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica >>".into(),
            format!("<< /Length {} >>\nstream\n{content}endstream", content.len()),
            format!("<< /Length {} >>\nstream\n{blue}endstream", blue.len()),
        ];
        let mut bytes = b"%PDF-1.4\n".to_vec();
        let mut offsets = Vec::new();
        for (index, object) in objects.iter().enumerate() {
            offsets.push(bytes.len());
            writeln!(bytes, "{} 0 obj\n{object}\nendobj", index + 1).unwrap();
        }
        let xref = bytes.len();
        writeln!(bytes, "xref\n0 {}\n0000000000 65535 f ", offsets.len() + 1).unwrap();
        for offset in &offsets {
            writeln!(bytes, "{offset:010} 00000 n ").unwrap();
        }
        writeln!(
            bytes,
            "trailer\n<< /Root 1 0 R /Size {} >>\nstartxref\n{xref}\n%%EOF",
            offsets.len() + 1
        )
        .unwrap();
        bytes
    }
}
