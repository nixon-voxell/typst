//! Exporting into _PDF_ documents.

use std::cmp::Eq;
use std::collections::HashMap;
use std::hash::Hash;

use fontdock::FaceId;
use image::{DynamicImage, GenericImageView, Rgba};
use pdf_writer::{
    CidFontType, ColorSpace, Content, FontFlags, Name, PdfWriter, Rect, Ref, Str,
    SystemInfo, UnicodeCmap,
};
use ttf_parser::{name_id, GlyphId};

use crate::env::{Env, ResourceId};
use crate::geom::Length;
use crate::layout::{BoxLayout, LayoutElement};

/// Export a list of layouts into a _PDF_ document.
///
/// This creates one page per layout. Additionally to the layouts, you need to
/// pass in the font loader used for typesetting such that the fonts can be
/// included in the _PDF_.
///
/// Returns the raw bytes making up the _PDF_ document.
pub fn export(layouts: &[BoxLayout], env: &Env) -> Vec<u8> {
    PdfExporter::new(layouts, env).write()
}

struct PdfExporter<'a> {
    writer: PdfWriter,
    layouts: &'a [BoxLayout],
    env: &'a Env,
    refs: Refs,
    fonts: Remapper<FaceId>,
    images: Remapper<ResourceId>,
}

impl<'a> PdfExporter<'a> {
    fn new(layouts: &'a [BoxLayout], env: &'a Env) -> Self {
        let mut writer = PdfWriter::new(1, 7);
        writer.set_indent(2);

        let mut fonts = Remapper::new();
        let mut images = Remapper::new();
        let mut alpha_masks = 0;

        for layout in layouts {
            for (_, element) in &layout.elements {
                match element {
                    LayoutElement::Text(shaped) => fonts.insert(shaped.face),
                    LayoutElement::Image(image) => {
                        let buf = env.resources.get_loaded::<DynamicImage>(image.res);
                        if buf.color().has_alpha() {
                            alpha_masks += 1;
                        }
                        images.insert(image.res);
                    }
                }
            }
        }

        let refs = Refs::new(layouts.len(), fonts.len(), images.len(), alpha_masks);

        Self {
            writer,
            layouts,
            env,
            refs,
            fonts,
            images,
        }
    }

    fn write(mut self) -> Vec<u8> {
        self.write_structure();
        self.write_pages();
        self.write_fonts();
        self.write_images();
        self.writer.finish(self.refs.catalog)
    }

    fn write_structure(&mut self) {
        // The document catalog.
        self.writer.catalog(self.refs.catalog).pages(self.refs.page_tree);

        // The root page tree.
        let mut pages = self.writer.pages(self.refs.page_tree);
        pages.kids(self.refs.pages());

        let mut resources = pages.resources();
        let mut fonts = resources.fonts();
        for (refs, f) in self.refs.fonts().zip(self.fonts.pdf_indices()) {
            let name = format!("F{}", f);
            fonts.pair(Name(name.as_bytes()), refs.type0_font);
        }

        drop(fonts);

        let mut images = resources.x_objects();
        for (id, im) in self.refs.images().zip(self.images.pdf_indices()) {
            let name = format!("Im{}", im);
            images.pair(Name(name.as_bytes()), id);
        }

        drop(images);
        drop(resources);
        drop(pages);

        // The page objects (non-root nodes in the page tree).
        for ((page_id, content_id), page) in
            self.refs.pages().zip(self.refs.contents()).zip(self.layouts)
        {
            self.writer
                .page(page_id)
                .parent(self.refs.page_tree)
                .media_box(Rect::new(
                    0.0,
                    0.0,
                    page.size.width.to_pt() as f32,
                    page.size.height.to_pt() as f32,
                ))
                .contents(content_id);
        }
    }

    fn write_pages(&mut self) {
        for (id, page) in self.refs.contents().zip(self.layouts) {
            self.write_page(id, &page);
        }
    }

    fn write_page(&mut self, id: Ref, page: &'a BoxLayout) {
        let mut content = Content::new();

        // We only write font switching actions when the used face changes. To
        // do that, we need to remember the active face.
        let mut face = FaceId::MAX;
        let mut size = Length::ZERO;

        let mut text = content.text();
        for (pos, element) in &page.elements {
            if let LayoutElement::Text(shaped) = element {
                // Check if we need to issue a font switching action.
                if shaped.face != face || shaped.font_size != size {
                    face = shaped.face;
                    size = shaped.font_size;

                    let name = format!("F{}", self.fonts.map(shaped.face));
                    text.font(Name(name.as_bytes()), size.to_pt() as f32);
                }

                let x = pos.x.to_pt() as f32;
                let y = (page.size.height - pos.y - size).to_pt() as f32;
                text.matrix(1.0, 0.0, 0.0, 1.0, x, y);
                text.show(&shaped.encode_glyphs_be());
            }
        }

        drop(text);

        for (pos, element) in &page.elements {
            if let LayoutElement::Image(image) = element {
                let name = format!("Im{}", self.images.map(image.res));
                let size = image.size;
                let x = pos.x.to_pt() as f32;
                let y = (page.size.height - pos.y - size.height).to_pt() as f32;
                let w = size.width.to_pt() as f32;
                let h = size.height.to_pt() as f32;

                content.save_state();
                content.matrix(w, 0.0, 0.0, h, x, y);
                content.x_object(Name(name.as_bytes()));
                content.restore_state();
            }
        }

        self.writer.stream(id, &content.finish());
    }

    fn write_fonts(&mut self) {
        for (refs, face_id) in self.refs.fonts().zip(self.fonts.layout_indices()) {
            let owned_face = self.env.fonts.get_loaded(face_id);
            let face = owned_face.get();

            let name = face
                .names()
                .find(|entry| {
                    entry.name_id() == name_id::POST_SCRIPT_NAME && entry.is_unicode()
                })
                .and_then(|entry| entry.to_string())
                .unwrap_or_else(|| "unknown".to_string());

            let base_font = format!("ABCDEF+{}", name);
            let base_font = Name(base_font.as_bytes());
            let cmap_name = Name(b"Custom");
            let system_info = SystemInfo {
                registry: Str(b"Adobe"),
                ordering: Str(b"Identity"),
                supplement: 0,
            };

            let mut flags = FontFlags::empty();
            flags.set(FontFlags::SERIF, name.contains("Serif"));
            flags.set(FontFlags::FIXED_PITCH, face.is_monospaced());
            flags.set(FontFlags::ITALIC, face.is_italic());
            flags.insert(FontFlags::SYMBOLIC);
            flags.insert(FontFlags::SMALL_CAP);

            // Convert from OpenType font units to PDF glyph units.
            let em_per_unit = 1.0 / face.units_per_em().unwrap_or(1000) as f32;
            let convert = |font_unit: f32| (1000.0 * em_per_unit * font_unit).round();
            let convert_i16 = |font_unit: i16| convert(font_unit as f32);
            let convert_u16 = |font_unit: u16| convert(font_unit as f32);

            let global_bbox = face.global_bounding_box();
            let bbox = Rect::new(
                convert_i16(global_bbox.x_min),
                convert_i16(global_bbox.y_min),
                convert_i16(global_bbox.x_max),
                convert_i16(global_bbox.y_max),
            );

            let italic_angle = face.italic_angle().unwrap_or(0.0);
            let ascender = convert_i16(face.typographic_ascender().unwrap_or(0));
            let descender = convert_i16(face.typographic_descender().unwrap_or(0));
            let cap_height = face.capital_height().map(convert_i16);
            let stem_v = 10.0 + 0.244 * (f32::from(face.weight().to_number()) - 50.0);

            // Write the base font object referencing the CID font.
            self.writer
                .type0_font(refs.type0_font)
                .base_font(base_font)
                .encoding_predefined(Name(b"Identity-H"))
                .descendant_font(refs.cid_font)
                .to_unicode(refs.cmap);

            // Write the CID font referencing the font descriptor.
            self.writer
                .cid_font(refs.cid_font, CidFontType::Type2)
                .base_font(base_font)
                .system_info(system_info)
                .font_descriptor(refs.font_descriptor)
                .widths()
                .individual(0, {
                    let num_glyphs = face.number_of_glyphs();
                    (0 .. num_glyphs).map(|g| {
                        let advance = face.glyph_hor_advance(GlyphId(g));
                        convert_u16(advance.unwrap_or(0))
                    })
                });

            // Write the font descriptor (contains metrics about the font).
            self.writer
                .font_descriptor(refs.font_descriptor)
                .font_name(base_font)
                .font_flags(flags)
                .font_bbox(bbox)
                .italic_angle(italic_angle)
                .ascent(ascender)
                .descent(descender)
                .cap_height(cap_height.unwrap_or(ascender))
                .stem_v(stem_v)
                .font_file2(refs.data);

            // Write the to-unicode character map, which maps glyph ids back to
            // unicode codepoints to enable copying out of the PDF.
            self.writer
                .cmap_stream(refs.cmap, &{
                    let mut cmap = UnicodeCmap::new(cmap_name, system_info);
                    for subtable in face.character_mapping_subtables() {
                        subtable.codepoints(|n| {
                            if let Some(c) = std::char::from_u32(n) {
                                if let Some(g) = face.glyph_index(c) {
                                    cmap.pair(g.0, c);
                                }
                            }
                        })
                    }
                    cmap.finish()
                })
                .name(cmap_name)
                .system_info(system_info);

            // Write the face's bytes.
            self.writer.stream(refs.data, owned_face.data());
        }
    }

    fn write_images(&mut self) {
        let mut mask = 0;

        for (id, resource) in self.refs.images().zip(self.images.layout_indices()) {
            let buf = self.env.resources.get_loaded::<DynamicImage>(resource);
            let data = buf.to_rgb8().into_raw();

            let mut image = self.writer.image_stream(id, &data);
            image.width(buf.width() as i32);
            image.height(buf.height() as i32);
            image.color_space(ColorSpace::DeviceRGB);
            image.bits_per_component(8);

            // Add a second gray-scale image containing the alpha values if this
            // is image has an alpha channel.
            if buf.color().has_alpha() {
                let mask_id = self.refs.alpha_mask(mask);

                image.s_mask(mask_id);
                drop(image);

                let mut samples = vec![];
                for (_, _, Rgba([_, _, _, a])) in buf.pixels() {
                    samples.push(a);
                }

                self.writer
                    .image_stream(mask_id, &samples)
                    .width(buf.width() as i32)
                    .height(buf.height() as i32)
                    .color_space(ColorSpace::DeviceGray)
                    .bits_per_component(8);

                mask += 1;
            }
        }
    }
}

/// We need to know exactly which indirect reference id will be used for which
/// objects up-front to correctly declare the document catalogue, page tree and
/// so on. These offsets are computed in the beginning and stored here.
struct Refs {
    catalog: Ref,
    page_tree: Ref,
    pages_start: i32,
    contents_start: i32,
    fonts_start: i32,
    images_start: i32,
    alpha_masks_start: i32,
    end: i32,
}

struct FontRefs {
    type0_font: Ref,
    cid_font: Ref,
    font_descriptor: Ref,
    cmap: Ref,
    data: Ref,
}

impl Refs {
    const OBJECTS_PER_FONT: usize = 5;

    fn new(layouts: usize, fonts: usize, images: usize, alpha_masks: usize) -> Self {
        let catalog = 1;
        let page_tree = catalog + 1;
        let pages_start = page_tree + 1;
        let contents_start = pages_start + layouts as i32;
        let fonts_start = contents_start + layouts as i32;
        let images_start = fonts_start + (Self::OBJECTS_PER_FONT * fonts) as i32;
        let alpha_masks_start = images_start + images as i32;
        let end = alpha_masks_start + alpha_masks as i32;

        Self {
            catalog: Ref::new(catalog),
            page_tree: Ref::new(page_tree),
            pages_start,
            contents_start,
            fonts_start,
            images_start,
            alpha_masks_start,
            end,
        }
    }

    fn pages(&self) -> impl Iterator<Item = Ref> {
        (self.pages_start .. self.contents_start).map(Ref::new)
    }

    fn contents(&self) -> impl Iterator<Item = Ref> {
        (self.contents_start .. self.images_start).map(Ref::new)
    }

    fn fonts(&self) -> impl Iterator<Item = FontRefs> {
        (self.fonts_start .. self.images_start)
            .step_by(Self::OBJECTS_PER_FONT)
            .map(|id| FontRefs {
                type0_font: Ref::new(id),
                cid_font: Ref::new(id + 1),
                font_descriptor: Ref::new(id + 2),
                cmap: Ref::new(id + 3),
                data: Ref::new(id + 4),
            })
    }

    fn images(&self) -> impl Iterator<Item = Ref> {
        (self.images_start .. self.end).map(Ref::new)
    }

    fn alpha_mask(&self, i: usize) -> Ref {
        Ref::new(self.alpha_masks_start + i as i32)
    }
}

/// Used to assign new, consecutive PDF-internal indices to things.
struct Remapper<Index> {
    /// Forwards from the old indices to the new pdf indices.
    to_pdf: HashMap<Index, usize>,
    /// Backwards from the pdf indices to the old indices.
    to_layout: Vec<Index>,
}

impl<Index> Remapper<Index>
where
    Index: Copy + Eq + Hash,
{
    fn new() -> Self {
        Self {
            to_pdf: HashMap::new(),
            to_layout: vec![],
        }
    }

    fn len(&self) -> usize {
        self.to_layout.len()
    }

    fn insert(&mut self, index: Index) {
        let to_layout = &mut self.to_layout;
        self.to_pdf.entry(index).or_insert_with(|| {
            let pdf_index = to_layout.len();
            to_layout.push(index);
            pdf_index
        });
    }

    fn map(&self, index: Index) -> usize {
        self.to_pdf[&index]
    }

    fn pdf_indices(&self) -> impl Iterator<Item = usize> {
        0 .. self.to_pdf.len()
    }

    fn layout_indices(&self) -> impl Iterator<Item = Index> + '_ {
        self.to_layout.iter().copied()
    }
}
