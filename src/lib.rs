use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::fmt::Debug;
use std::fs::File;
use std::io::{self, Read, Write};
use std::num::ParseIntError;
use std::path::Path;
use std::str::FromStr;
use std::{fs, u32};

use bincode;
use byteorder::ByteOrder;
use byteorder::{LittleEndian, WriteBytesExt};
use csv::StringRecord;
use encoding::all::UTF_16LE;
use encoding::{DecoderTrap, Encoding};
use glob::glob;
use lindera_core::core::character_definition::{
    CategoryData, CategoryId, CharacterDefinitions, LookupTable,
};
use lindera_core::core::unknown_dictionary::UnknownDictionary;
use lindera_core::core::word_entry::{WordEntry, WordId};
use lindera_fst::MapBuilder;

#[derive(Debug)]
pub enum ParsingError {
    Encoding,
    IoError(io::Error),
    ContentError(String),
}

impl ParsingError {
    pub fn from_error<D: Debug>(error: D) -> ParsingError {
        ParsingError::ContentError(format!("{:?}", error))
    }
}

impl From<io::Error> for ParsingError {
    fn from(io_err: io::Error) -> Self {
        ParsingError::IoError(io_err)
    }
}

impl From<ParseIntError> for ParsingError {
    fn from(parse_err: ParseIntError) -> Self {
        ParsingError::from_error(parse_err)
    }
}

fn read_mecab_file(dir: &str, filename: &str) -> Result<String, ParsingError> {
    let path = Path::new(dir).join(Path::new(filename));
    let mut input_read = File::open(path)?;
    let mut buffer = Vec::new();
    input_read.read_to_end(&mut buffer)?;
    encoding::all::UTF_8
        .decode(&buffer, DecoderTrap::Strict)
        .map_err(|_| ParsingError::Encoding)
}

fn build_dict(input_dir: &str, output_dir: &str) -> Result<(), ParsingError> {
    println!("BUILD DICT");

    let mut filenames: Vec<String> = Vec::new();
    for entry in glob(format!("{}/*.csv", input_dir).as_str()).expect("Failed to read glob pattern")
    {
        match entry {
            Ok(path) => {
                filenames.push(path.file_name().unwrap().to_str().unwrap().to_string());
            }
            Err(e) => return Err(ParsingError::ContentError(format!("{}", e))),
        }
    }

    let mut rows: Vec<StringRecord> = Vec::new();
    for filename in filenames {
        println!("reading csv: {}", filename);

        let mut rdr = csv::ReaderBuilder::new()
            .has_headers(false)
            .from_path(Path::new(input_dir).join(Path::new(filename.as_str())))
            .unwrap();

        for result in rdr.records() {
            let record = result.unwrap();
            rows.push(record);
        }
    }

    println!("sorting entries");
    rows.sort_by_key(|row| row[0].to_string().clone());

    let wtr_fst = io::BufWriter::new(
        File::create(Path::new(output_dir).join(Path::new("dict.fst"))).unwrap(),
    );
    let mut wtr_vals = io::BufWriter::new(
        File::create(Path::new(output_dir).join(Path::new("dict.vals"))).unwrap(),
    );

    let mut word_entry_map: BTreeMap<String, Vec<WordEntry>> = BTreeMap::new();

    for (row_id, row) in rows.iter().enumerate() {
        word_entry_map
            .entry(row[0].to_string())
            .or_insert_with(Vec::new)
            .push(WordEntry {
                word_id: WordId(row_id as u32),
                word_cost: i16::from_str(row[3].trim()).unwrap(),
                cost_id: u16::from_str(row[1].trim()).unwrap(),
            });
    }

    let mut wtr_words = io::BufWriter::new(
        File::create(Path::new(output_dir).join(Path::new("dict.words"))).unwrap(),
    );
    let mut wtr_words_idx = io::BufWriter::new(
        File::create(Path::new(output_dir).join(Path::new("dict.wordsidx"))).unwrap(),
    );
    let mut words_buffer = Vec::new();
    for row in rows.iter() {
        let word = vec![
            row[4].to_string(),
            row[5].to_string(),
            row[6].to_string(),
            row[7].to_string(),
            row[8].to_string(),
            row[9].to_string(),
            row[10].to_string(),
            row[11].to_string(),
        ];
        let offset = words_buffer.len();
        wtr_words_idx.write_u32::<LittleEndian>(offset as u32)?;
        bincode::serialize_into(&mut words_buffer, &word).unwrap();
    }

    wtr_words.write_all(&words_buffer[..])?;
    wtr_words.flush()?;
    wtr_words_idx.flush()?;

    let mut id = 0u64;

    println!("building fst");
    let mut fst_build = MapBuilder::new(wtr_fst).unwrap();
    for (key, word_entries) in &word_entry_map {
        let len = word_entries.len() as u64;
        let val = (id << 5) | len;
        fst_build.insert(&key, val).unwrap();
        id += len;
    }
    fst_build.finish().unwrap();

    println!("building values");
    for word_entries in word_entry_map.values() {
        for word_entry in word_entries {
            word_entry.serialize(&mut wtr_vals)?;
        }
    }
    wtr_vals.flush().unwrap();

    Ok(())
}

fn build_cost_matrix(input_dir: &str, output_dir: &str) -> Result<(), ParsingError> {
    println!("BUILD COST MATRIX");
    let matrix_data = read_mecab_file(input_dir, "matrix.def")?;
    let mut lines = Vec::new();
    for line in matrix_data.lines() {
        let fields: Vec<i32> = line
            .split_whitespace()
            .map(i32::from_str)
            .collect::<Result<_, _>>()?;
        lines.push(fields);
    }
    let mut lines_it = lines.into_iter();
    let header = lines_it.next().unwrap();
    let forward_size = header[0] as u32;
    let backward_size = header[1] as u32;
    let len = 2 + (forward_size * backward_size) as usize;
    let mut costs = vec![i16::max_value(); len];
    costs[0] = forward_size as i16;
    costs[1] = backward_size as i16;
    for fields in lines_it {
        let forward_id = fields[0] as u32;
        let backward_id = fields[1] as u32;
        let cost = fields[2] as u16;
        costs[2 + (backward_id + forward_id * backward_size) as usize] = cost as i16;
    }

    let mut wtr = io::BufWriter::new(File::create(
        Path::new(output_dir).join(Path::new("matrix.mtx")),
    )?);
    for cost in costs {
        wtr.write_i16::<LittleEndian>(cost)?;
    }
    wtr.flush()?;
    Ok(())
}

const DEFAULT_CATEGORY_NAME: &'static str = "DEFAULT";

#[derive(Default)]
pub struct CharacterDefinitionsBuilder {
    category_definition: Vec<CategoryData>,
    category_index: HashMap<String, CategoryId>,
    char_ranges: Vec<(u32, u32, Vec<CategoryId>)>,
}

fn ucs2_to_unicode(ucs2_codepoint: u16) -> u32 {
    let mut buf = [0u8; 2];
    LittleEndian::write_u16(&mut buf[..], ucs2_codepoint);
    let s: String = UTF_16LE.decode(&buf[..], DecoderTrap::Strict).unwrap();
    let chrs: Vec<char> = s.chars().collect();
    assert_eq!(chrs.len(), 1);
    chrs[0] as u32
}

fn parse_hex_codepoint(s: &str) -> Result<u32, ParsingError> {
    let removed_0x = s.trim_start_matches("0x");
    let ucs2_codepoint = u16::from_str_radix(removed_0x, 16).map_err(ParsingError::from_error)?;
    let utf8_str = ucs2_to_unicode(ucs2_codepoint);
    Ok(utf8_str)
}

impl CharacterDefinitionsBuilder {
    pub fn category_id(&mut self, category_name: &str) -> CategoryId {
        let num_categories = self.category_index.len();
        *self
            .category_index
            .entry(category_name.to_string())
            .or_insert(CategoryId(num_categories))
    }

    fn lookup_categories(&self, c: u32, categories_buffer: &mut Vec<CategoryId>) {
        categories_buffer.clear();
        for (start, stop, category_ids) in &self.char_ranges {
            if *start <= c && *stop >= c {
                for cat in category_ids {
                    if !categories_buffer.contains(cat) {
                        categories_buffer.push(*cat);
                    }
                }
            }
        }
        if categories_buffer.is_empty() {
            let default_category = self.category_index.get(DEFAULT_CATEGORY_NAME).unwrap();
            categories_buffer.push(*default_category);
        }
    }

    fn build_lookup_table(&self) -> LookupTable<CategoryId> {
        let boundaries_set: BTreeSet<u32> = self
            .char_ranges
            .iter()
            .flat_map(|(low, high, _)| vec![*low, *high + 1u32])
            .collect();
        let boundaries: Vec<u32> = boundaries_set.into_iter().collect();
        LookupTable::from_fn(boundaries, &|c, buff| self.lookup_categories(c, buff))
    }

    pub fn parse(&mut self, content: &String) -> Result<(), ParsingError> {
        for line in content.lines() {
            let line_str = line.split('#').next().unwrap().trim();
            if line_str.is_empty() {
                continue;
            }
            if line_str.starts_with("0x") {
                self.parse_range(line_str)?;
            } else {
                self.parse_category(line_str)?;
            }
        }
        Ok(())
    }

    fn parse_range(&mut self, line: &str) -> Result<(), ParsingError> {
        let fields: Vec<&str> = line.split_whitespace().collect();
        let range_bounds: Vec<&str> = fields[0].split("..").collect();
        let lower_bound: u32;
        let higher_bound: u32;
        match range_bounds.len() {
            1 => {
                lower_bound = parse_hex_codepoint(range_bounds[0])?;
                higher_bound = lower_bound;
            }
            2 => {
                lower_bound = parse_hex_codepoint(range_bounds[0])?;
                // the right bound is included in the file.
                higher_bound = parse_hex_codepoint(range_bounds[1])?;
            }
            _ => {
                return Err(ParsingError::ContentError(format!(
                    "Invalid line: {}",
                    line
                )));
            }
        }
        let category_ids: Vec<CategoryId> = fields[1..]
            .iter()
            .map(|category| self.category_id(category))
            .collect();
        println!("{} - {} => {:?}", lower_bound, higher_bound, &fields[1..]);
        self.char_ranges
            .push((lower_bound, higher_bound, category_ids));
        Ok(())
    }

    fn parse_category(&mut self, line: &str) -> Result<(), ParsingError> {
        let fields = line.split_ascii_whitespace().collect::<Vec<&str>>();
        if fields.len() != 4 {
            return Err(ParsingError::ContentError(format!(
                "Expected 4 fields. Got {} in {}",
                fields.len(),
                line
            )));
        }
        let invoke = fields[1].parse::<u32>().map_err(ParsingError::from_error)? == 1;
        let group = fields[2].parse::<u32>().map_err(ParsingError::from_error)? == 1;
        let length = fields[3].parse::<u32>().map_err(ParsingError::from_error)?;
        let category_data = CategoryData {
            invoke,
            group,
            length,
        };
        // force a category_id allocation
        self.category_id(fields[0]);
        self.category_definition.push(category_data);
        Ok(())
    }

    pub fn build(self) -> CharacterDefinitions {
        let mut category_names: Vec<String> = (0..self.category_index.len())
            .map(|_| String::new())
            .collect();
        for (category_name, category_id) in &self.category_index {
            category_names[category_id.0] = category_name.clone();
        }
        let mapping = self.build_lookup_table();
        CharacterDefinitions {
            category_definitions: self.category_definition,
            category_names,
            mapping,
        }
    }
}

#[derive(Debug)]
pub struct DictionaryEntry {
    surface: String,
    left_id: u32,
    right_id: u32,
    word_cost: i32,
}

fn parse_dictionary_entry(fields: &[&str]) -> Result<DictionaryEntry, ParsingError> {
    if fields.len() != 12 {
        return Err(ParsingError::ContentError(format!(
            "Invalid number of fields. Expect 12, got {}",
            fields.len()
        )));
    }
    let surface = fields[0];
    let left_id = u32::from_str(fields[1])?;
    let right_id = u32::from_str(fields[2])?;
    let word_cost = i32::from_str(fields[3])?;
    Ok(DictionaryEntry {
        surface: surface.to_string(),
        left_id,
        right_id,
        word_cost,
    })
}

fn make_costs_array(entries: &[DictionaryEntry]) -> Vec<WordEntry> {
    entries
        .iter()
        .map(|e| {
            // the following `assert_eq!()` is ignored because `left_id` and `right_id` registered
            // in `unk.def` are not the same.
            //assert_eq!(e.left_id, e.right_id);
            WordEntry {
                word_id: WordId(std::u32::MAX),
                cost_id: e.left_id as u16,
                word_cost: e.word_cost as i16,
            }
        })
        .collect()
}

fn get_entry_id_matching_surface(entries: &[DictionaryEntry], target_surface: &str) -> Vec<u32> {
    entries
        .iter()
        .enumerate()
        .filter_map(|(entry_id, entry)| {
            if entry.surface == target_surface.to_string() {
                Some(entry_id as u32)
            } else {
                None
            }
        })
        .collect()
}

fn make_category_references(categories: &[String], entries: &[DictionaryEntry]) -> Vec<Vec<u32>> {
    categories
        .iter()
        .map(|category| get_entry_id_matching_surface(entries, category))
        .collect()
}

fn parse_unk(
    categories: &[String],
    file_content: &String,
) -> Result<UnknownDictionary, ParsingError> {
    let mut unknown_dict_entries = Vec::new();
    for line in file_content.lines() {
        let fields: Vec<&str> = line.split(",").collect::<Vec<&str>>();
        let entry = parse_dictionary_entry(&fields[..])?;
        unknown_dict_entries.push(entry);
    }

    let category_references = make_category_references(categories, &unknown_dict_entries[..]);
    let costs = make_costs_array(&unknown_dict_entries[..]);
    Ok(UnknownDictionary {
        category_references,
        costs,
    })
}

fn build_chardef(input_dir: &str, output_dir: &str) -> Result<CharacterDefinitions, ParsingError> {
    println!("BUILD CHARDEF");
    let mut char_definitions_builder = CharacterDefinitionsBuilder::default();
    let char_def = read_mecab_file(input_dir, "char.def")?;
    char_definitions_builder.parse(&char_def)?;
    let char_definitions = char_definitions_builder.build();
    let mut wtr_chardef = io::BufWriter::new(File::create(
        Path::new(output_dir).join(Path::new("char_def.bin")),
    )?);
    bincode::serialize_into(&mut wtr_chardef, &char_definitions)
        .map_err(ParsingError::from_error)?;
    wtr_chardef.flush()?;
    Ok(char_definitions)
}

fn build_unk(
    input_dir: &str,
    chardef: &CharacterDefinitions,
    output_dir: &str,
) -> Result<(), ParsingError> {
    println!("BUILD UNK");
    let unk_data = read_mecab_file(input_dir, "unk.def")?;
    let unknown_dictionary = parse_unk(&chardef.categories(), &unk_data)?;
    let mut wtr_unk = io::BufWriter::new(File::create(
        Path::new(output_dir).join(Path::new("unk.bin")),
    )?);
    bincode::serialize_into(&mut wtr_unk, &unknown_dictionary).map_err(ParsingError::from_error)?;
    wtr_unk.flush()?;
    Ok(())
}

pub fn build(input_dir: &str, output_dir: &str) -> Result<(), String> {
    fs::create_dir_all(&output_dir).unwrap_or_default();

    let chardef = build_chardef(input_dir, output_dir).unwrap();
    build_unk(input_dir, &chardef, output_dir).unwrap();
    build_dict(input_dir, output_dir).unwrap();
    build_cost_matrix(input_dir, output_dir).unwrap();

    Ok(())
}
