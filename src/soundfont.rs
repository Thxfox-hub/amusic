//! GM soundfont playback via rustysynth. Renders notes to buffers and plays as samples.

use std::path::Path;
use std::sync::Arc;

use crate::sample_loader::Sample;
use rustysynth::{SoundFont, Synthesizer, SynthesizerSettings};

/// GM program numbers (first preset in each bank is 0-127).
/// Table complète General MIDI ; les anciens noms restent en alias en fin de liste.
const GM_PROGRAM: &[(&str, u8)] = &[
    // Pianos
    ("gm_piano", 0),
    ("gm_piano_bright", 1),
    ("gm_piano_electric_grand", 2),
    ("gm_honky_tonk", 3),
    ("gm_epiano1", 4),
    ("gm_epiano2", 5),
    ("gm_harpsichord", 6),
    ("gm_clavinet", 7),
    // Percussions chromatiques
    ("gm_celesta", 8),
    ("gm_glockenspiel", 9),
    ("gm_music_box", 10),
    ("gm_vibraphone", 11),
    ("gm_marimba", 12),
    ("gm_xylophone", 13),
    ("gm_tubular_bells", 14),
    ("gm_dulcimer", 15),
    // Orgues
    ("gm_organ_drawbar", 16),
    ("gm_organ_percussive", 17),
    ("gm_organ_rock", 18),
    ("gm_organ_church", 19),
    ("gm_organ_reed", 20),
    ("gm_accordion", 21),
    ("gm_harmonica", 22),
    ("gm_tango_accordion", 23),
    // Guitares
    ("gm_acoustic_guitar_nylon", 24),
    ("gm_acoustic_guitar_steel", 25),
    ("gm_electric_guitar_jazz", 26),
    ("gm_electric_guitar_clean", 27),
    ("gm_electric_guitar_muted", 28),
    ("gm_guitar_overdriven", 29),
    ("gm_guitar_distortion", 30),
    ("gm_guitar_harmonics", 31),
    // Basses — dont les deux basses synthé GM (techno)
    ("gm_acoustic_bass", 32),
    ("gm_electric_bass", 33),
    ("gm_electric_bass_pick", 34),
    ("gm_fretless_bass", 35),
    ("gm_slap_bass_1", 36),
    ("gm_slap_bass_2", 37),
    ("gm_synth_bass_1", 38),
    ("gm_synth_bass_2", 39),
    // Cordes solistes
    ("gm_violin", 40),
    ("gm_viola", 41),
    ("gm_cello", 42),
    ("gm_contrabass", 43),
    ("gm_tremolo_strings", 44),
    ("gm_pizzicato_strings", 45),
    ("gm_harp", 46),
    ("gm_timpani", 47),
    // Ensembles
    ("gm_strings_1", 48),
    ("gm_strings_2", 49),
    ("gm_synth_strings_1", 50),
    ("gm_synth_strings_2", 51),
    ("gm_choir_aahs", 52),
    ("gm_voice_oohs", 53),
    ("gm_synth_voice", 54),
    ("gm_orchestra_hit", 55),
    // Cuivres
    ("gm_trumpet", 56),
    ("gm_trombone", 57),
    ("gm_tuba", 58),
    ("gm_muted_trumpet", 59),
    ("gm_french_horn", 60),
    ("gm_brass_section", 61),
    ("gm_synth_brass_1", 62),
    ("gm_synth_brass_2", 63),
    // Anches
    ("gm_soprano_sax", 64),
    ("gm_alto_sax", 65),
    ("gm_tenor_sax", 66),
    ("gm_baritone_sax", 67),
    ("gm_oboe", 68),
    ("gm_english_horn", 69),
    ("gm_bassoon", 70),
    ("gm_clarinet", 71),
    // Flûtes
    ("gm_piccolo", 72),
    ("gm_flute", 73),
    ("gm_recorder", 74),
    ("gm_pan_flute", 75),
    ("gm_blown_bottle", 76),
    ("gm_shakuhachi", 77),
    ("gm_whistle", 78),
    ("gm_ocarina", 79),
    // Leads synthétiques
    ("gm_lead_square", 80),
    ("gm_lead_sawtooth", 81),
    ("gm_lead_calliope", 82),
    ("gm_lead_chiff", 83),
    ("gm_lead_charang", 84),
    ("gm_lead_voice", 85),
    ("gm_lead_fifths", 86),
    ("gm_lead_bass", 87),
    // Nappes synthétiques
    ("gm_pad_new_age", 88),
    ("gm_pad_warm", 89),
    ("gm_pad_polysynth", 90),
    ("gm_pad_choir", 91),
    ("gm_pad_bowed", 92),
    ("gm_pad_metallic", 93),
    ("gm_pad_halo", 94),
    ("gm_pad_sweep", 95),
    // Effets synthétiques
    ("gm_fx_rain", 96),
    ("gm_fx_soundtrack", 97),
    ("gm_fx_crystal", 98),
    ("gm_fx_atmosphere", 99),
    ("gm_fx_brightness", 100),
    ("gm_fx_goblins", 101),
    ("gm_fx_echoes", 102),
    ("gm_fx_sci_fi", 103),
    // Ethniques
    ("gm_sitar", 104),
    ("gm_banjo", 105),
    ("gm_shamisen", 106),
    ("gm_koto", 107),
    ("gm_kalimba", 108),
    ("gm_bagpipe", 109),
    ("gm_fiddle", 110),
    ("gm_shanai", 111),
    // Percussifs
    ("gm_tinkle_bell", 112),
    ("gm_agogo", 113),
    ("gm_steel_drums", 114),
    ("gm_woodblock", 115),
    ("gm_taiko_drum", 116),
    ("gm_melodic_tom", 117),
    ("gm_synth_drum", 118),
    ("gm_reverse_cymbal", 119),
    // Effets sonores
    ("gm_guitar_fret_noise", 120),
    ("gm_breath_noise", 121),
    ("gm_seashore", 122),
    ("gm_bird_tweet", 123),
    ("gm_telephone", 124),
    ("gm_helicopter", 125),
    ("gm_applause", 126),
    ("gm_gunshot", 127),
    // Alias historiques (code existant)
    ("gm_epiano_1", 4),
    ("gm_epiano_2", 5),
];

/// Tous les noms `gm_*` exposés — pour la palette « Sons ».
pub fn gm_sound_names() -> Vec<&'static str> {
    GM_PROGRAM.iter().map(|(n, _)| *n).collect()
}

fn gm_name_to_program(name: &str) -> Option<u8> {
    let name_lower = name.to_lowercase();
    GM_PROGRAM
        .iter()
        .find(|(n, _)| *n == name_lower)
        .map(|(_, p)| *p)
}

/// Load a .sf2 file. Returns None if file missing or invalid.
pub fn load_soundfont(path: &Path) -> Option<Arc<SoundFont>> {
    let data = std::fs::read(path).ok()?;
    let mut cursor = std::io::Cursor::new(data);
    let sf = SoundFont::new(&mut cursor).ok()?;
    Some(Arc::new(sf))
}

/// Render a single GM note into a stereo sample (interleaved L,R).
pub fn render_gm_note(
    soundfont: &Arc<SoundFont>,
    program: u8,
    midi_note: u8,
    velocity: u8,
    duration_secs: f32,
    sample_rate: u32,
) -> Option<Sample> {
    let settings = SynthesizerSettings::new(sample_rate as i32);
    let mut syn = Synthesizer::new(soundfont, &settings).ok()?;
    let block_size = syn.get_block_size();
    if block_size == 0 {
        return None;
    }

    // Сброс канала и выбор GM банка/пресета (для FluidR3 GS и др.).
    syn.process_midi_message(0, 0xB0, 0x79, 0); // Reset All Controllers
    syn.process_midi_message(0, 0xB0, 0, 0); // Bank Select MSB = 0
    syn.process_midi_message(0, 0xB0, 32, 0); // Bank Select LSB = 0
    syn.process_midi_message(0, 0xC0, program as i32, 0); // Program Change
    syn.note_on(0, midi_note as i32, velocity as i32);

    let total_frames = (duration_secs * sample_rate as f32).ceil() as usize;
    let total_samples = total_frames * 2; // stereo interleaved
    let mut left = vec![0f32; block_size];
    let mut right = vec![0f32; block_size];
    let mut out: Vec<f32> = Vec::with_capacity(total_samples);

    let mut rendered = 0usize;
    while rendered < total_frames {
        syn.render(&mut left, &mut right);
        let to_take = (total_frames - rendered).min(block_size);
        for i in 0..to_take {
            out.push(left[i]);
            out.push(right[i]);
        }
        rendered += to_take;
        if rendered >= total_frames {
            break;
        }
    }

    syn.note_off(0, midi_note as i32);

    Some(Sample {
        data: Arc::new(out),
        sample_rate,
        channels: 2,
    })
}

/// If `name` is a known GM sound name, return its program number.
pub fn gm_program_for_name(name: &str) -> Option<u8> {
    gm_name_to_program(name)
}

