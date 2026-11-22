use crate::random::Random;

/// The tables LadybugDB needs before the data loads. RhizoDB is schemaless and skips them.
///
/// Every node carries an integer `id` because LadybugDB requires a primary key. RhizoDB stores it
/// as an ordinary property, so both engines hold the same properties.
pub const LADYBUG_SCHEMA: [&str; 4] = [
    "CREATE NODE TABLE Person(id INT64 PRIMARY KEY, name STRING, age INT64)",
    "CREATE NODE TABLE City(id INT64 PRIMARY KEY, name STRING, population INT64)",
    "CREATE REL TABLE KNOWS(FROM Person TO Person, since INT64)",
    "CREATE REL TABLE LIVES_IN(FROM Person TO City)",
];

const NAMES: [&str; 6] = ["Ada", "Alan", "Barbara", "Donald", "Edsger", "Grace"];
const CITIES: [&str; 4] = ["Lund", "Oslo", "Riga", "Turku"];

/// A person node. A missing age is a property the node does not have.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Person {
    /// Primary key.
    pub id: i64,
    /// Name, drawn from a small pool so names repeat.
    pub name: String,
    /// Age, absent for some people.
    pub age: Option<i64>,
}

/// A city node.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct City {
    /// Primary key, disjoint from person ids.
    pub id: i64,
    /// Name.
    pub name: String,
    /// Population.
    pub population: i64,
}

/// A `KNOWS` edge between two people. Self loops and parallel edges occur.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Knows {
    /// Id of the source person.
    pub from: i64,
    /// Id of the target person.
    pub to: i64,
    /// Year the edge started.
    pub since: i64,
}

/// A `LIVES_IN` edge from a person to a city.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LivesIn {
    /// Id of the person.
    pub person: i64,
    /// Id of the city.
    pub city: i64,
}

/// A generated graph that both engines load.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Graph {
    /// Person nodes.
    pub people: Vec<Person>,
    /// City nodes.
    pub cities: Vec<City>,
    /// `KNOWS` edges.
    pub knows: Vec<Knows>,
    /// `LIVES_IN` edges.
    pub lives_in: Vec<LivesIn>,
}

/// Size of a generated graph.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Size {
    /// Number of people.
    pub people: usize,
    /// Number of `KNOWS` edges.
    pub knows: usize,
}

impl Graph {
    /// Generates a graph from `seed`. The same seed and size always give the same graph.
    pub fn generate(seed: u64, size: Size) -> Graph {
        let mut random = Random::new(seed);
        let people: Vec<Person> = (1..)
            .take(size.people)
            .map(|id| Person {
                id,
                name: random.pick(&NAMES).copied().unwrap_or_default().to_string(),
                age: (!random.chance(1, 5)).then(|| random.between(18, 80)),
            })
            .collect();
        let city_ids = 1001..;
        let cities: Vec<City> = city_ids
            .zip(CITIES)
            .map(|(id, name)| City {
                id,
                name: name.to_string(),
                population: random.between(1, 50) * 1000,
            })
            .collect();
        let ids: Vec<i64> = people.iter().map(|person| person.id).collect();
        let knows = (0..size.knows)
            .filter_map(|_| {
                Some(Knows {
                    from: *random.pick(&ids)?,
                    to: *random.pick(&ids)?,
                    since: random.between(1990, 2024),
                })
            })
            .collect();
        let city_ids: Vec<i64> = cities.iter().map(|city| city.id).collect();
        let lives_in = ids
            .iter()
            .filter_map(|&person| {
                let homeless = random.chance(1, 6);
                let city = *random.pick(&city_ids)?;
                (!homeless).then_some(LivesIn { person, city })
            })
            .collect();
        Graph {
            people,
            cities,
            knows,
            lives_in,
        }
    }

    /// The statements that load the graph. Both engines accept them unchanged, after
    /// [`LADYBUG_SCHEMA`] on LadybugDB.
    pub fn statements(&self) -> Vec<String> {
        let people = self.people.iter().map(|person| match person.age {
            Some(age) => format!(
                "CREATE (:Person {{id: {}, name: '{}', age: {age}}})",
                person.id, person.name
            ),
            None => format!(
                "CREATE (:Person {{id: {}, name: '{}'}})",
                person.id, person.name
            ),
        });
        let cities = self.cities.iter().map(|city| {
            format!(
                "CREATE (:City {{id: {}, name: '{}', population: {}}})",
                city.id, city.name, city.population
            )
        });
        let knows = self.knows.iter().map(|edge| {
            format!(
                "MATCH (a:Person {{id: {}}}), (b:Person {{id: {}}}) CREATE (a)-[:KNOWS {{since: {}}}]->(b)",
                edge.from, edge.to, edge.since
            )
        });
        let lives_in = self.lives_in.iter().map(|edge| {
            format!(
                "MATCH (a:Person {{id: {}}}), (c:City {{id: {}}}) CREATE (a)-[:LIVES_IN]->(c)",
                edge.person, edge.city
            )
        });
        people.chain(cities).chain(knows).chain(lives_in).collect()
    }
}

#[cfg(test)]
#[path = "graph.test.rs"]
mod tests;
