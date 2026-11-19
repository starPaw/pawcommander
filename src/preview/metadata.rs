use crate::i18n::text;
use eframe::egui::{self, RichText};

const TEXT_CAPACITY: usize = 256;

#[repr(C)]
struct NativeExif {
    make: [u8; TEXT_CAPACITY],
    model: [u8; TEXT_CAPACITY],
    lens: [u8; TEXT_CAPACITY],
    taken: [u8; TEXT_CAPACITY],
    iso: f64,
    aperture: f64,
    exposure: f64,
    focal_length: f64,
}

unsafe extern "C" {
    fn paw_image_exif(bytes: *const u8, length: usize, out: *mut NativeExif);
}

#[derive(Default, PartialEq)]
pub(super) struct Exif {
    camera: Option<String>,
    lens: Option<String>,
    taken: Option<String>,
    iso: Option<f64>,
    aperture: Option<f64>,
    exposure: Option<f64>,
    focal_length: Option<f64>,
}

impl Exif {
    pub(super) fn read(bytes: &[u8]) -> Option<Box<Self>> {
        let mut native = NativeExif {
            make: [0; TEXT_CAPACITY],
            model: [0; TEXT_CAPACITY],
            lens: [0; TEXT_CAPACITY],
            taken: [0; TEXT_CAPACITY],
            iso: 0.0,
            aperture: 0.0,
            exposure: 0.0,
            focal_length: 0.0,
        };
        // ImageIO borrows these bytes for the duration of the call and copies
        // the selected fields into the initialized output structure.
        unsafe { paw_image_exif(bytes.as_ptr(), bytes.len(), &mut native) };
        let camera = match (exif_text(&native.make), exif_text(&native.model)) {
            (Some(make), Some(model)) if !model.starts_with(&make) => {
                Some(format!("{make} {model}"))
            }
            (make, model) => model.or(make),
        };
        let exif = Self {
            camera,
            lens: exif_text(&native.lens),
            taken: exif_text(&native.taken),
            iso: positive(native.iso),
            aperture: positive(native.aperture),
            exposure: positive(native.exposure),
            focal_length: positive(native.focal_length),
        };
        (exif != Self::default()).then(|| Box::new(exif))
    }

    fn fields(&self, ui: &mut egui::Ui) {
        field(ui, "Камера", self.camera.as_deref());
        field(ui, "Объектив", self.lens.as_deref());
        field(ui, "Дата съёмки", self.taken.as_deref());
        field(
            ui,
            "ISO",
            self.iso.map(|value| format!("{value:.0}")).as_deref(),
        );
        field(
            ui,
            "Диафрагма",
            self.aperture
                .map(|value| format!("f/{value:.1}"))
                .as_deref(),
        );
        field(
            ui,
            "Выдержка",
            self.exposure
                .map(|value| format!("{value} {}", text("с")))
                .as_deref(),
        );
        field(
            ui,
            "Фокусное расстояние",
            self.focal_length
                .map(|value| format!("{value} {}", text("мм")))
                .as_deref(),
        );
    }
}

fn exif_text(buffer: &[u8]) -> Option<String> {
    let end = buffer.iter().position(|&byte| byte == 0)?;
    let value = std::str::from_utf8(&buffer[..end]).ok()?;
    let value: String = value.chars().filter(|ch| !ch.is_control()).collect();
    let value = value.trim();
    (!value.is_empty()).then(|| value.to_owned())
}

pub(super) fn positive(value: f64) -> Option<f64> {
    (value.is_finite() && value > 0.0).then_some(value)
}

pub(super) fn dimensions([width, height]: [u32; 2]) -> String {
    format!("{width} × {height}")
}

pub(super) fn show_image(ui: &mut egui::Ui, size: [u32; 2], exif: Option<&Exif>) {
    section(ui, |ui| {
        field(ui, "Разрешение", Some(&dimensions(size)));
        exif_fields(ui, exif);
    });
}

pub(super) fn exif_fields(ui: &mut egui::Ui, exif: Option<&Exif>) {
    if let Some(exif) = exif {
        exif.fields(ui);
    } else {
        field(ui, "EXIF", None);
    }
}

pub(super) fn section(ui: &mut egui::Ui, fields: impl FnOnce(&mut egui::Ui)) {
    let height = (ui.ctx().content_rect().height() * 0.4).clamp(36.0, 180.0);
    egui::CollapsingHeader::new(text("Метаданные")).show(ui, |ui| {
        egui::ScrollArea::vertical()
            .id_salt("metadata-rows")
            .max_height(height)
            .auto_shrink([false, true])
            .show(ui, |ui| {
                egui::Grid::new("metadata-fields")
                    .num_columns(2)
                    .min_col_width(88.0)
                    .spacing([12.0, 5.0])
                    .show(ui, fields);
            });
    });
}

pub(super) fn field(ui: &mut egui::Ui, label: &'static str, value: Option<&str>) {
    let label = ui.label(RichText::new(text(label)).weak());
    ui.add(
        egui::Label::new(value.unwrap_or_else(|| text("Недоступно")))
            .wrap()
            .selectable(true),
    )
    .labelled_by(label.id);
    ui.end_row();
}
