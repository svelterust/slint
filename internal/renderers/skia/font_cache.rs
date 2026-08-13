// Copyright © SixtyFPS GmbH <info@slint.dev>
// SPDX-License-Identifier: GPL-3.0-only OR LicenseRef-Slint-Royalty-free-2.0 OR LicenseRef-Slint-Software-3.0

use clru::CLruCache;
use i_slint_common::sharedfontique::HashedBlob;
use i_slint_core::textlayout::sharedparley::{fontique, parley};
use std::cell::RefCell;
use std::collections::HashMap;
use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::num::NonZeroUsize;

const FONT_CACHE_CAPACITY: NonZeroUsize = NonZeroUsize::new(64).unwrap();

pub struct FontCache {
    font_mgr: skia_safe::FontMgr,
    // Use HashedBlob in key to keep strong reference to font data blob,
    // preventing eviction from fontique's shared cache (see commit 30a03cf).
    // The u64 is a hash of variation settings (0 for base typefaces).
    fonts: CLruCache<(HashedBlob, u32, u64), Option<skia_safe::Typeface>>,
}

impl Default for FontCache {
    fn default() -> Self {
        Self { font_mgr: skia_safe::FontMgr::new(), fonts: CLruCache::new(FONT_CACHE_CAPACITY) }
    }
}

impl FontCache {
    pub fn font_with_variations(
        &mut self,
        font: &parley::FontData,
        synthesis: &fontique::Synthesis,
    ) -> Option<skia_safe::Typeface> {
        let variation_settings = synthesis.variation_settings();

        let mut variations_hash = 0u64;
        if !variation_settings.is_empty() {
            let mut hasher = DefaultHasher::new();
            for &(tag, value) in variation_settings {
                tag.to_be_bytes().hash(&mut hasher);
                value.to_bits().hash(&mut hasher);
            }
            variations_hash = hasher.finish();
        }

        let key = (font.data.clone().into(), font.index, variations_hash);

        if let Some(cached) = self.fonts.get(&key) {
            return cached.clone();
        }

        let mut typeface = self.load_typeface_internal(font);

        if !variation_settings.is_empty() {
            typeface = typeface.and_then(|base| {
                let coords: Vec<skia_safe::font_arguments::variation_position::Coordinate> =
                    variation_settings
                        .iter()
                        .map(|&(tag, value)| {
                            skia_safe::font_arguments::variation_position::Coordinate {
                                axis: skia_safe::FourByteTag::new(u32::from_be_bytes(
                                    tag.to_be_bytes(),
                                )),
                                value,
                            }
                        })
                        .collect();
                let position =
                    skia_safe::font_arguments::VariationPosition { coordinates: &coords };
                let args = skia_safe::FontArguments::new().set_variation_design_position(position);
                base.clone_with_arguments(&args).or(Some(base))
            });
        }

        self.fonts.put(key, typeface.clone());
        typeface
    }

    fn load_typeface_internal(&self, font: &parley::FontData) -> Option<skia_safe::Typeface> {
        let typeface = self.font_mgr.new_from_data(
            font.data.as_ref(),
            if font.index > 0 { Some(font.index as _) } else { None },
        );

        // Due to  https://issues.skia.org/issues/310510989, fonts from true type collections
        // with an index > 0 fail to load on macOS. As a workaround, we manually extract the font from the
        // collection and load it as a single font.
        #[cfg(target_vendor = "apple")]
        if font.index > 0
            && typeface.is_none()
            && let Some(typeface) = read_fonts::CollectionRef::new(font.data.as_ref())
                .ok()
                .and_then(|ttc| ttc.get(font.index).ok())
                .map(|ttf| write_fonts::FontBuilder::new().copy_missing_tables(ttf).build())
                .and_then(|new_ttf| self.font_mgr.new_from_data(&new_ttf, None))
        {
            return Some(typeface);
        }

        typeface
    }
}

thread_local! {
    pub static FONT_CACHE: RefCell<FontCache> = RefCell::new(Default::default())
}

#[derive(Clone)]
pub struct BitmapGlyphImage {
    pub image: Option<skia_safe::Image>,
    pub x: f32,
    pub y: f32,
}

#[derive(Default)]
struct BitmapFontCache {
    fonts: HashMap<(usize, usize, u32), Vec<&'static i_slint_core::graphics::BitmapFont>>,
    glyphs: HashMap<(usize, i16, u16), Option<BitmapGlyphImage>>,
}

impl BitmapFontCache {
    fn register(&mut self, font: &'static i_slint_core::graphics::BitmapFont) {
        let data = font.source_data.as_slice();
        let fonts =
            self.fonts.entry((data.as_ptr() as usize, data.len(), font.face_index)).or_default();
        if !fonts.iter().any(|registered| core::ptr::eq(*registered, font)) {
            fonts.push(font);
        }
    }

    fn glyph(
        &mut self,
        font: &parley::FontData,
        pixel_size: i16,
        glyph_id: u16,
    ) -> Option<BitmapGlyphImage> {
        let data = font.data.as_ref();
        let bitmap_font = self
            .fonts
            .get(&(data.as_ptr() as usize, data.len(), font.index))?
            .iter()
            .copied()
            .find(|font| {
                font.packed
                    && !font.sdf
                    && font.glyphs.iter().any(|set| {
                        set.pixel_size == pixel_size
                            && set.glyph_data.iter().any(|glyph| glyph.glyph_id == glyph_id)
                    })
            })?;

        let key = (bitmap_font as *const _ as usize, pixel_size, glyph_id);
        if let Some(glyph) = self.glyphs.get(&key) {
            return glyph.clone();
        }

        let glyph = bitmap_font
            .glyphs
            .iter()
            .find(|set| set.pixel_size == pixel_size)
            .and_then(|set| set.glyph_data.iter().find(|glyph| glyph.glyph_id == glyph_id))
            .and_then(|glyph| {
                let width = usize::try_from(glyph.width).ok()?;
                let height = usize::try_from(glyph.height).ok()?;
                if width == 0 || height == 0 {
                    return Some(BitmapGlyphImage { image: None, x: 0., y: 0. });
                }
                let row_stride = width.div_ceil(8);
                let packed = glyph.data.as_slice();
                if packed.len() != row_stride * height {
                    return None;
                }
                let mut alpha = vec![0; width * height];
                for y in 0..height {
                    for x in 0..width {
                        alpha[y * width + x] =
                            if packed[y * row_stride + x / 8] & (0x80 >> (x % 8)) != 0 {
                                255
                            } else {
                                0
                            };
                    }
                }
                let info = skia_safe::ImageInfo::new(
                    (width as i32, height as i32),
                    skia_safe::ColorType::Alpha8,
                    skia_safe::AlphaType::Premul,
                    None,
                );
                skia_safe::images::raster_from_data(&info, skia_safe::Data::new_copy(&alpha), width)
                    .map(|image| BitmapGlyphImage {
                        image: Some(image),
                        x: glyph.x as f32 / 64.,
                        y: -(height as f32 + glyph.y as f32 / 64.),
                    })
            });
        self.glyphs.insert(key, glyph.clone());
        glyph
    }
}

thread_local! {
    static BITMAP_FONT_CACHE: RefCell<BitmapFontCache> = RefCell::default()
}

pub fn register_bitmap_font(font: &'static i_slint_core::graphics::BitmapFont) {
    BITMAP_FONT_CACHE.with_borrow_mut(|cache| cache.register(font));
}

pub fn bitmap_glyph(
    font: &parley::FontData,
    pixel_size: i16,
    glyph_id: u16,
) -> Option<BitmapGlyphImage> {
    BITMAP_FONT_CACHE.with_borrow_mut(|cache| cache.glyph(font, pixel_size, glyph_id))
}
