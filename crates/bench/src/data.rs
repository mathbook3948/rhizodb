use std::collections::HashSet;
use std::fs;
use std::fs::File;
use std::io;
use std::io::BufWriter;
use std::io::Write;
use std::path::Path;

use crate::random::Random;
use crate::scale::Plan;

const FIRST_NAMES: [&str; 40] = [
    "Ada",
    "Alan",
    "Barbara",
    "Claude",
    "Donald",
    "Edsger",
    "Frances",
    "Grace",
    "Hedy",
    "Ivan",
    "Jean",
    "Katherine",
    "Linus",
    "Margaret",
    "Niklaus",
    "Ole",
    "Peter",
    "Radia",
    "Sophie",
    "Tim",
    "Ursula",
    "Vint",
    "Whitfield",
    "Xavier",
    "Yukihiro",
    "Zuzana",
    "Anita",
    "Bjarne",
    "Carol",
    "Dennis",
    "Eva",
    "Fei",
    "Guido",
    "Hana",
    "Ines",
    "Joan",
    "Ken",
    "Lena",
    "Marvin",
    "Nina",
];

const LAST_NAMES: [&str; 25] = [
    "Lovelace",
    "Turing",
    "Liskov",
    "Shannon",
    "Knuth",
    "Dijkstra",
    "Allen",
    "Hopper",
    "Lamarr",
    "Sutherland",
    "Sammet",
    "Johnson",
    "Torvalds",
    "Hamilton",
    "Wirth",
    "Dahl",
    "Naur",
    "Perlman",
    "Wilson",
    "Lee",
    "Le Guin",
    "Cerf",
    "Diffie",
    "Matsumoto",
    "Borg",
];

const VOCABULARY: &[&str] = &[
    "graph", "node", "edge", "query", "index", "storage", "memory", "cache", "thread", "lock",
    "commit", "write", "read", "scan", "join", "path", "label", "property", "vector", "search",
    "keyword", "text", "score", "rank", "model", "schema", "table", "column", "row", "key",
    "value", "type", "list", "map", "set", "sort", "group", "count", "sum", "average", "stream",
    "batch", "file", "disk", "page", "buffer", "log", "crash", "recover", "snapshot", "version",
    "reader", "writer", "friend", "city", "music", "travel", "coffee", "book", "garden", "river",
    "mountain", "ocean", "forest", "winter", "summer", "bridge", "market", "train",
];

const RARE_WORDS: [(&str, usize); 4] = [("quokka", 1), ("zephyr", 3), ("umbra", 6), ("lychee", 9)];

/// How many results a search returns.
pub const SEARCH_LIMIT: usize = 10;

const KOREAN_WORDS: [&str; 6] = ["그래프", "데이터", "서울", "사람", "도시", "검색"];

pub const PERSONS_CSV: &str = "persons.csv";
pub const CITIES_CSV: &str = "cities.csv";
pub const KNOWS_CSV: &str = "knows.csv";
pub const LIVES_IN_CSV: &str = "lives_in.csv";

const BIO_WORDS: usize = 20;
const EMBEDDING_DIMENSION: usize = 64;
const PROBES: usize = 2_000;
const QUERY_TERMS: usize = 16;

/// A person node. A missing age is a property the node does not have.
#[derive(Clone, Debug, PartialEq)]
pub struct Person {
    pub id: i64,
    pub name: String,
    pub age: Option<i64>,
    pub bio: String,
    pub embedding: Vec<f64>,
}

/// A city node.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct City {
    pub id: i64,
    pub name: String,
}

/// A `KNOWS` edge between two persons, by person id.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Knows {
    pub from: i64,
    pub to: i64,
    pub since: i64,
}

/// A `LIVES_IN` edge from a person to a city, by id.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LivesIn {
    pub person: i64,
    pub city: i64,
}

/// How many nodes and edges of each kind a load covers, counted from the start of the dataset.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Loaded {
    pub persons: usize,
    pub cities: usize,
    pub knows: usize,
    pub lives_in: usize,
}

/// What a loaded database holds, as counts and sums a query can read back.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Facts {
    pub persons: u64,
    pub cities: u64,
    pub knows: u64,
    pub lives_in: u64,
    pub aged: u64,
    pub age_sum: i64,
}

/// The graph both engines load, and the probes the workloads draw from it.
#[derive(Clone, Debug, PartialEq)]
pub struct Dataset {
    pub people: Vec<Person>,
    pub cities: Vec<City>,
    pub knows: Vec<Knows>,
    pub lives_in: Vec<LivesIn>,
    pub probes: Vec<i64>,
    pub terms: Vec<String>,
    pub query_vectors: Vec<Vec<f64>>,
    /// For each of `terms`, the ids of every person whose bio holds the term as a word.
    pub keyword_truth: Vec<Vec<i64>>,
    /// For each of `query_vectors`, the ids of the persons nearest by cosine distance, nearest
    /// first, found by comparing against every person.
    pub vector_truth: Vec<Vec<i64>>,
}

impl Dataset {
    /// Generates a dataset from `seed`. The same seed and plan always give the same dataset.
    ///
    /// Person ids run from 1 and city ids from 1 in creation order. A `KNOWS` edge leaves a
    /// uniformly drawn person and enters a person drawn with a skew toward the lowest ids, which
    /// makes those persons hubs. No pair of persons has two `KNOWS` edges the same way. A tenth of the persons have no `LIVES_IN` edge, and a fifth have
    /// no age. An embedding holds multiples of 1/64, which both engines store without rounding.
    pub fn generate(seed: u64, plan: Plan) -> Dataset {
        let mut random = Random::new(seed);
        let persons = plan.persons;
        let mut people: Vec<Person> = (1..=persons as i64)
            .map(|id| Person {
                id,
                name: format!(
                    "{} {}",
                    pick(&mut random, &FIRST_NAMES),
                    pick(&mut random, &LAST_NAMES)
                ),
                age: (!random.chance(1, 5)).then(|| random.between(18, 80)),
                bio: bio(&mut random),
                embedding: embedding(&mut random),
            })
            .collect();
        let cities = (1..=plan.cities as i64)
            .map(|id| City {
                id,
                name: format!("City {id}"),
            })
            .collect();
        let mut seen = HashSet::new();
        let knows = (0..persons * plan.knows_per_person)
            .map(|_| {
                let from = random.below(persons);
                let mut to = random.skewed(persons, 3);
                let mut attempts = 0;
                while to == from || !seen.insert((from, to)) {
                    attempts += 1;
                    to = if attempts < 32 {
                        random.skewed(persons, 3)
                    } else {
                        (to + 1) % persons
                    };
                }
                Knows {
                    from: from as i64 + 1,
                    to: to as i64 + 1,
                    since: random.between(1990, 2024),
                }
            })
            .collect();
        let lives_in = (1..=persons as i64)
            .filter_map(|person| {
                let city = random.below(plan.cities) as i64 + 1;
                (!random.chance(1, 10)).then_some(LivesIn { person, city })
            })
            .collect();
        let probes = (0..PROBES)
            .map(|_| random.below(persons) as i64 + 1)
            .collect();
        let common: Vec<&str> = (0..QUERY_TERMS)
            .map(|_| VOCABULARY[random.skewed(VOCABULARY.len(), 2)])
            .collect();
        let query_vectors: Vec<Vec<f64>> =
            (0..QUERY_TERMS).map(|_| embedding(&mut random)).collect();
        let mut terms: Vec<String> = RARE_WORDS
            .iter()
            .map(|(word, _)| (*word).to_owned())
            .collect();
        terms.extend(common.into_iter().map(str::to_owned));
        for (word, hits) in RARE_WORDS {
            for _ in 0..hits.min(persons) {
                if let Some(person) = people.get_mut(random.below(persons))
                    && !person.bio.split(' ').any(|found| found == word)
                {
                    person.bio.push(' ');
                    person.bio.push_str(word);
                }
            }
        }
        let keyword_truth = terms
            .iter()
            .map(|term| {
                people
                    .iter()
                    .filter(|person| person.bio.split(' ').any(|word| word == term))
                    .map(|person| person.id)
                    .collect()
            })
            .collect();
        let vector_truth = query_vectors
            .iter()
            .map(|vector| nearest(&people, vector, SEARCH_LIMIT))
            .collect();
        Dataset {
            people,
            cities,
            knows,
            lives_in,
            probes,
            terms,
            query_vectors,
            keyword_truth,
            vector_truth,
        }
    }

    /// What a database holds after it loads the prefixes of the dataset that `loaded` names.
    pub fn facts(&self, loaded: Loaded) -> Facts {
        let ages = || {
            self.people
                .iter()
                .take(loaded.persons)
                .filter_map(|p| p.age)
        };
        Facts {
            persons: loaded.persons.min(self.people.len()) as u64,
            cities: loaded.cities.min(self.cities.len()) as u64,
            knows: loaded.knows.min(self.knows.len()) as u64,
            lives_in: loaded.lives_in.min(self.lives_in.len()) as u64,
            aged: ages().count() as u64,
            age_sum: ages().sum(),
        }
    }

    /// Writes the dataset as four CSV files with a header row each, for every engine whose bulk
    /// loader reads files. A missing age is an empty field and an embedding is a quoted
    /// bracketed list. Each file is written under a temporary name and renamed when complete, and
    /// the call does nothing when the last file exists.
    pub fn write_csv(&self, dir: &Path) -> io::Result<()> {
        if dir.join(LIVES_IN_CSV).exists() {
            return Ok(());
        }
        fs::create_dir_all(dir)?;
        let mut persons = csv_file(dir, PERSONS_CSV, "id,name,age,bio,embedding")?;
        for person in &self.people {
            let age = person.age.map_or_else(String::new, |age| age.to_string());
            let embedding: Vec<String> = person.embedding.iter().map(f64::to_string).collect();
            writeln!(
                persons,
                "{},{},{},{},\"[{}]\"",
                person.id,
                person.name,
                age,
                person.bio,
                embedding.join(",")
            )?;
        }
        finish_csv(dir, PERSONS_CSV, persons)?;
        let mut cities = csv_file(dir, CITIES_CSV, "id,name")?;
        for city in &self.cities {
            writeln!(cities, "{},{}", city.id, city.name)?;
        }
        finish_csv(dir, CITIES_CSV, cities)?;
        let mut knows = csv_file(dir, KNOWS_CSV, "from,to,since")?;
        for edge in &self.knows {
            writeln!(knows, "{},{},{}", edge.from, edge.to, edge.since)?;
        }
        finish_csv(dir, KNOWS_CSV, knows)?;
        let mut lives_in = csv_file(dir, LIVES_IN_CSV, "from,to")?;
        for edge in &self.lives_in {
            writeln!(lives_in, "{},{}", edge.person, edge.city)?;
        }
        finish_csv(dir, LIVES_IN_CSV, lives_in)
    }

    /// The prefixes that cover the whole dataset.
    pub fn loaded_in_full(&self) -> Loaded {
        Loaded {
            persons: self.people.len(),
            cities: self.cities.len(),
            knows: self.knows.len(),
            lives_in: self.lives_in.len(),
        }
    }
}

fn csv_file(dir: &Path, name: &str, header: &str) -> io::Result<BufWriter<File>> {
    let mut file = BufWriter::new(File::create(dir.join(format!("{name}.tmp")))?);
    writeln!(file, "{header}")?;
    Ok(file)
}

fn finish_csv(dir: &Path, name: &str, mut file: BufWriter<File>) -> io::Result<()> {
    file.flush()?;
    drop(file);
    fs::rename(dir.join(format!("{name}.tmp")), dir.join(name))
}

fn nearest(people: &[Person], vector: &[f64], count: usize) -> Vec<i64> {
    let norm = |values: &[f64]| values.iter().map(|value| value * value).sum::<f64>().sqrt();
    let query_norm = norm(vector);
    let mut distances: Vec<(f64, i64)> = people
        .iter()
        .map(|person| {
            let dot: f64 = person
                .embedding
                .iter()
                .zip(vector)
                .map(|(a, b)| a * b)
                .sum();
            (
                1.0 - dot / (norm(&person.embedding) * query_norm),
                person.id,
            )
        })
        .collect();
    distances.sort_by(|a, b| a.0.total_cmp(&b.0).then(a.1.cmp(&b.1)));
    distances
        .into_iter()
        .take(count)
        .map(|(_, id)| id)
        .collect()
}

fn pick<'a>(random: &mut Random, items: &[&'a str]) -> &'a str {
    random.pick(items).copied().unwrap_or_default()
}

fn bio(random: &mut Random) -> String {
    let words: Vec<&str> = (0..BIO_WORDS)
        .map(|_| {
            if random.chance(1, 12) {
                pick(random, &KOREAN_WORDS)
            } else {
                VOCABULARY[random.skewed(VOCABULARY.len(), 2)]
            }
        })
        .collect();
    words.join(" ")
}

fn embedding(random: &mut Random) -> Vec<f64> {
    (0..EMBEDDING_DIMENSION)
        .map(|_| random.between(-63, 64) as f64 / 64.0)
        .collect()
}

#[cfg(test)]
#[path = "data.test.rs"]
mod tests;
