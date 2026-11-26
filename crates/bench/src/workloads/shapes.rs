use crate::engine::EngineError;
use crate::engine::EngineKind;
use crate::engine::Query;
use crate::engine::Texts;
use crate::measure::Measurement;
use crate::workloads::Comparison;
use crate::workloads::Context;
use crate::workloads::Needs;
use crate::workloads::Workload;
use crate::workloads::run_queries;

const HUB: i64 = 1;
const ID_RANGE: i64 = 100;
const IDS_PER_QUERY: usize = 20;
const PAGE: i64 = 50;
const PAGES: usize = 10;

const TWO_HOP_FILTERED: Texts = Texts::graph(
    "MATCH (a:Person {id: $id})-[k1:KNOWS]->(b:Person)-[k2:KNOWS]->(c:Person) WHERE k1.since >= 2000 AND b.age > 25 AND k2.since < 2018 AND c.age < 60 RETURN count(*)",
)
.sql(
    "SELECT count(*) FROM knows k1 JOIN persons b ON b.id = k1.dst JOIN knows k2 ON k2.src = k1.dst JOIN persons c ON c.id = k2.dst WHERE k1.src = $id AND k1.since >= 2000 AND b.age > 25 AND k2.since < 2018 AND c.age < 60",
);
const THREE_HOP_FILTERED: Texts = Texts::graph(
    "MATCH (a:Person {id: $id})-[k1:KNOWS]->(b:Person)-[k2:KNOWS]->(c:Person)-[k3:KNOWS]->(d:Person) WHERE k1.since >= 1995 AND b.age >= 20 AND k2.since >= 2000 AND c.age <= 70 AND k3.since < 2020 AND d.age > 30 AND d.id <> a.id AND (c.id <> a.id OR d.id <> b.id) RETURN count(DISTINCT d)",
)
.sql(
    "SELECT count(DISTINCT k3.dst) FROM knows k1 JOIN persons b ON b.id = k1.dst JOIN knows k2 ON k2.src = k1.dst JOIN persons c ON c.id = k2.dst JOIN knows k3 ON k3.src = k2.dst JOIN persons d ON d.id = k3.dst WHERE k1.src = $id AND k1.since >= 1995 AND b.age >= 20 AND k2.since >= 2000 AND c.age <= 70 AND k3.since < 2020 AND d.age > 30 AND d.id <> $id AND (k3.src <> k1.src OR k3.dst <> k1.dst)",
);
const OPTIONAL_CHAIN: Texts = Texts::graph(
    "MATCH (p:Person {id: $id}) OPTIONAL MATCH (p)-[:LIVES_IN]->(c:City) OPTIONAL MATCH (p)-[:KNOWS]->(f:Person)-[:LIVES_IN]->(c) RETURN p.name, c.name, count(f)",
)
.sql(
    "SELECT p.name, c.name, count(f.src) FROM persons p LEFT JOIN lives_in l ON l.src = p.id LEFT JOIN cities c ON c.id = l.dst LEFT JOIN (SELECT k.src, l2.dst AS city FROM knows k JOIN lives_in l2 ON l2.src = k.dst) f ON f.src = p.id AND f.city = c.id WHERE p.id = $id GROUP BY p.id, p.name, c.name",
);
const OPTIONAL_SCAN: Texts = Texts::graph(
    "MATCH (p:Person) OPTIONAL MATCH (p)-[:LIVES_IN]->(c:City) RETURN count(p), count(c)",
)
.sql("SELECT count(*), count(l.dst) FROM persons p LEFT JOIN lives_in l ON l.src = p.id");
const OPTIONAL_FRIENDS: Texts = Texts::graph(
    "MATCH (a:Person {id: $id})-[:KNOWS]->(f:Person) OPTIONAL MATCH (f)-[:LIVES_IN]->(c:City) OPTIONAL MATCH (f)-[:KNOWS]->(g:Person) WHERE g.age > 70 RETURN f.id, c.name, count(g)",
)
.sql(
    "SELECT k.dst, c.name, count(g.id) FROM knows k LEFT JOIN lives_in l ON l.src = k.dst LEFT JOIN cities c ON c.id = l.dst LEFT JOIN knows k2 ON k2.src = k.dst LEFT JOIN persons g ON g.id = k2.dst AND g.age > 70 WHERE k.src = $id GROUP BY k.dst, c.name",
);

const IN_DEGREE_GROUPS: Texts =
    Texts::graph("MATCH (a:Person)-[:KNOWS]->(b:Person) RETURN b.id, count(a)")
        .sql("SELECT dst, count(*) FROM knows GROUP BY dst");
const NAME_GROUPS: Texts =
    Texts::graph("MATCH (p:Person) RETURN p.name, count(*), min(p.age), max(p.age)")
        .sql("SELECT name, count(*), min(age), max(age) FROM persons GROUP BY name");
const CITY_SINCE_GROUPS: Texts = Texts::graph(
    "MATCH (a:Person)-[k:KNOWS]->(b:Person)-[:LIVES_IN]->(c:City) RETURN c.id, k.since, count(*)",
)
.sql(
    "SELECT l.dst, k.since, count(*) FROM knows k JOIN lives_in l ON l.src = k.dst GROUP BY l.dst, k.since",
);
const FRIEND_AGE_GROUPS: Texts = Texts::graph(
    "MATCH (a:Person)-[:KNOWS]->(b:Person) WHERE b.age IS NOT NULL RETURN a.id, count(b), avg(b.age)",
)
.sql(
    "SELECT k.src, count(*), avg(b.age) FROM knows k JOIN persons b ON b.id = k.dst WHERE b.age IS NOT NULL GROUP BY k.src",
);

const ORDER_COMPUTED: Texts = Texts::graph(
    "MATCH (p:Person) WHERE p.age IS NOT NULL RETURN p.id, p.age * 7 % 13 + p.id % 5 AS score ORDER BY score DESC, p.id LIMIT 20",
)
.sql(
    "SELECT id, age * 7 % 13 + id % 5 AS score FROM persons WHERE age IS NOT NULL ORDER BY score DESC, id LIMIT 20",
);
const ORDER_DEGREE: Texts = Texts::graph(
    "MATCH (p:Person)<-[:KNOWS]-(f:Person) RETURN p.id, count(f) AS degree ORDER BY degree DESC, p.id LIMIT 10",
)
.sql("SELECT dst, count(*) AS degree FROM knows GROUP BY dst ORDER BY degree DESC, dst LIMIT 10");
const ORDER_LENGTH: Texts = Texts::graph(
    "MATCH (p:Person) RETURN p.id, size(p.bio) AS length ORDER BY length DESC, p.id LIMIT 10",
)
.sql("SELECT id, length(bio) AS length FROM persons ORDER BY length DESC, id LIMIT 10");

const SKIP_DEEP: Texts =
    Texts::graph("MATCH (p:Person) RETURN p.id, p.name ORDER BY p.id SKIP $skip LIMIT 20")
        .sql("SELECT id, name FROM persons ORDER BY id LIMIT 20 OFFSET $skip");
const PAGES_BY_NAME: Texts =
    Texts::graph("MATCH (p:Person) RETURN p.id, p.name ORDER BY p.name, p.id SKIP $skip LIMIT 50")
        .sql("SELECT id, name FROM persons ORDER BY name, id LIMIT 50 OFFSET $skip");
const HUB_FOLLOWERS_PAGE: Texts = Texts::graph(
    "MATCH (h:Person {id: $hub})<-[k:KNOWS]-(f:Person) RETURN f.id, k.since ORDER BY k.since DESC, f.id SKIP $skip LIMIT 20",
)
.sql(
    "SELECT src, since FROM knows WHERE dst = $hub ORDER BY since DESC, src LIMIT 20 OFFSET $skip",
);

const TOP_PER_CITY: Texts = Texts::graph(
    "MATCH (p:Person)-[:LIVES_IN]->(c:City) WHERE p.age IS NOT NULL WITH c, p ORDER BY p.age DESC, p.id WITH c, collect(p.id) AS ids UNWIND ids[0..3] AS id RETURN c.id, id",
)
.sql(
    "SELECT city, id FROM (SELECT l.dst AS city, p.id AS id, row_number() OVER (PARTITION BY l.dst ORDER BY p.age DESC, p.id) AS position FROM persons p JOIN lives_in l ON l.src = p.id WHERE p.age IS NOT NULL) WHERE position <= 3",
)
.overriding(&[(
    EngineKind::Ladybugdb,
    "MATCH (p:Person)-[:LIVES_IN]->(c:City) WHERE p.age IS NOT NULL WITH c, p ORDER BY p.age DESC, p.id SKIP 0 WITH c, collect(p.id) AS ids UNWIND list_slice(ids, 1, 3) AS id RETURN c.id, id",
)]);
const LATEST_FRIENDS: Texts = Texts::graph(
    "MATCH (a:Person)-[k:KNOWS]->(b:Person) WHERE a.id <= $last WITH a, b, k ORDER BY k.since DESC, b.id WITH a, collect(b.id) AS ids UNWIND ids[0..2] AS id RETURN a.id, id",
)
.sql(
    "SELECT src, dst FROM (SELECT src, dst, row_number() OVER (PARTITION BY src ORDER BY since DESC, dst) AS position FROM knows WHERE src <= $last) WHERE position <= 2",
)
.overriding(&[(
    EngineKind::Ladybugdb,
    "MATCH (a:Person)-[k:KNOWS]->(b:Person) WHERE a.id <= $last WITH a, b, k ORDER BY k.since DESC, b.id SKIP 0 WITH a, collect(b.id) AS ids UNWIND list_slice(ids, 1, 2) AS id RETURN a.id, id",
)]);
const OLDEST_PER_CITY: Texts = Texts::graph(
    "MATCH (p:Person)-[:LIVES_IN]->(c:City) WITH c, max(p.age) AS oldest MATCH (q:Person)-[:LIVES_IN]->(c) WHERE q.age = oldest RETURN c.id, oldest, count(q)",
)
.sql(
    "SELECT l.dst, m.oldest, count(*) FROM lives_in l JOIN persons q ON q.id = l.src JOIN (SELECT l2.dst AS city, max(p.age) AS oldest FROM lives_in l2 JOIN persons p ON p.id = l2.src GROUP BY l2.dst) m ON m.city = l.dst WHERE q.age = m.oldest GROUP BY l.dst, m.oldest",
);

const COLLECT_UNWIND_FRIENDS: Texts = Texts::graph(
    "MATCH (a:Person {id: $id})-[:KNOWS]->(b:Person) WITH collect(b.id) AS ids UNWIND ids AS fid MATCH (f:Person {id: fid})-[:KNOWS]->(c:Person) WHERE c.id <> $id RETURN count(DISTINCT c)",
)
.sql(
    "SELECT count(DISTINCT k2.dst) FROM knows k1 JOIN knows k2 ON k2.src = k1.dst WHERE k1.src = $id AND k2.dst <> $id",
);
const COLLECT_UNWIND_GROUPS: Texts = Texts::graph(
    "MATCH (p:Person)-[:LIVES_IN]->(c:City) WITH c, collect(p.age) AS ages UNWIND ages AS age RETURN c.id, count(age), sum(age)",
)
.sql(
    "SELECT l.dst, count(p.age), sum(p.age) FROM lives_in l JOIN persons p ON p.id = l.src GROUP BY l.dst",
);
const COLLECT_SIZES: Texts = Texts::graph(
    "MATCH (a:Person)-[:KNOWS]->(b:Person) WITH a, collect(b.id) AS friends RETURN size(friends) AS n, count(*)",
)
.sql("SELECT n, count(*) FROM (SELECT src, count(*) AS n FROM knows GROUP BY src) GROUP BY n");

const EXISTS_OLD_FRIEND: Texts = Texts::graph(
    "MATCH (p:Person) WHERE EXISTS { MATCH (p)-[:KNOWS]->(f:Person) WHERE f.age > 75 } RETURN count(*)",
)
.sql(
    "SELECT count(*) FROM persons p WHERE EXISTS (SELECT 1 FROM knows k JOIN persons f ON f.id = k.dst WHERE k.src = p.id AND f.age > 75)",
);
const NOT_EXISTS_CITY: Texts = Texts::graph(
    "MATCH (p:Person) WHERE NOT EXISTS { MATCH (p)-[:LIVES_IN]->(:City) } RETURN count(*)",
)
.sql(
    "SELECT count(*) FROM persons p WHERE NOT EXISTS (SELECT 1 FROM lives_in l WHERE l.src = p.id)",
);
const COUNT_FILTER: Texts = Texts::graph(
    "MATCH (p:Person) WHERE COUNT { MATCH (p)<-[:KNOWS]-(:Person) } >= 20 RETURN count(*)",
)
.sql("SELECT count(*) FROM (SELECT dst FROM knows GROUP BY dst HAVING count(*) >= 20)");
const COUNT_PROJECT: Texts = Texts::graph(
    "MATCH (p:Person {id: $id}) RETURN p.name, COUNT { MATCH (p)-[:KNOWS]->(:Person)-[:KNOWS]->(:Person) }",
)
.sql(
    "SELECT p.name, (SELECT count(*) FROM knows k1 JOIN knows k2 ON k2.src = k1.dst WHERE k1.src = p.id) FROM persons p WHERE p.id = $id",
);
const RECOMMEND: Texts = Texts::graph(
    "MATCH (a:Person {id: $id})-[:KNOWS]->(:Person)-[:KNOWS]->(c:Person) WHERE c.id <> $id AND NOT EXISTS { MATCH (a)-[:KNOWS]->(c) } RETURN c.id, count(*) AS mutual ORDER BY mutual DESC, c.id LIMIT 10",
)
.sql(
    "SELECT k2.dst, count(*) AS mutual FROM knows k1 JOIN knows k2 ON k2.src = k1.dst WHERE k1.src = $id AND k2.dst <> $id AND NOT EXISTS (SELECT 1 FROM knows k3 WHERE k3.src = $id AND k3.dst = k2.dst) GROUP BY k2.dst ORDER BY mutual DESC, k2.dst LIMIT 10",
);

const STARTS_WITH: Texts =
    Texts::graph("MATCH (p:Person) WHERE p.name STARTS WITH 'Grace' RETURN count(*)")
        .sql("SELECT count(*) FROM persons WHERE substr(name, 1, 5) = 'Grace'");
const ENDS_WITH: Texts =
    Texts::graph("MATCH (p:Person) WHERE p.name ENDS WITH 'son' RETURN count(*)")
        .sql("SELECT count(*) FROM persons WHERE substr(name, -3) = 'son'");
const LOWER_CONTAINS: Texts =
    Texts::graph("MATCH (p:Person) WHERE lower(p.name) CONTAINS 'an' RETURN count(*)")
        .sql("SELECT count(*) FROM persons WHERE instr(lower(name), 'an') > 0");
const INITIALS: Texts =
    Texts::graph("MATCH (p:Person) RETURN upper(substring(p.name, 0, 1)) AS initial, count(*)")
        .sql("SELECT upper(substr(name, 1, 1)) AS initial, count(*) FROM persons GROUP BY initial")
        .overriding(&[(
            EngineKind::Ladybugdb,
            "MATCH (p:Person) RETURN upper(substring(p.name, 1, 1)) AS initial, count(*)",
        )]);
const FIRST_NAMES: Texts = Texts::graph(
    "MATCH (p:Person) RETURN split(p.name, ' ')[0] AS first, count(*)",
)
.sql("SELECT substr(name, 1, instr(name, ' ') - 1) AS first, count(*) FROM persons GROUP BY first")
.overriding(&[(
    EngineKind::Ladybugdb,
    "MATCH (p:Person) RETURN string_split(p.name, ' ')[1] AS first, count(*)",
)]);
const CONCAT_DISTINCT: Texts = Texts::graph(
    "MATCH (p:Person) WHERE p.age IS NOT NULL RETURN count(DISTINCT p.name + ':' + toString(p.age))",
)
.sql("SELECT count(DISTINCT name || ':' || age) FROM persons WHERE age IS NOT NULL")
.overriding(&[(
    EngineKind::Ladybugdb,
    "MATCH (p:Person) WHERE p.age IS NOT NULL RETURN count(DISTINCT concat(p.name, ':', cast(p.age AS STRING)))",
)]);
const REGEX_NAME: Texts =
    Texts::graph("MATCH (p:Person) WHERE p.name =~ '[A-G].* [LTS].*' RETURN count(*)")
        .sql("SELECT count(*) FROM persons WHERE name GLOB '[A-G]* [LTS]*'");
const REGEX_BIO: Texts = Texts::graph(
    "MATCH (p:Person) WHERE p.bio =~ '.*(coffee|book) (garden|river).*' RETURN count(*)",
)
.sql(
    "SELECT count(*) FROM persons WHERE bio GLOB '*coffee garden*' OR bio GLOB '*coffee river*' OR bio GLOB '*book garden*' OR bio GLOB '*book river*'",
);

const ID_RANGE_SCAN: Texts =
    Texts::graph("MATCH (p:Person) WHERE p.id >= $low AND p.id < $high RETURN p.name")
        .sql("SELECT name FROM persons WHERE id >= $low AND id < $high");
const AGE_RANGE: Texts =
    Texts::graph("MATCH (p:Person) WHERE p.age >= 30 AND p.age <= 34 RETURN count(*), sum(p.age)")
        .sql("SELECT count(*), sum(age) FROM persons WHERE age >= 30 AND age <= 34");
const NAME_RANGE: Texts =
    Texts::graph("MATCH (p:Person) WHERE p.name >= 'K' AND p.name < 'L' RETURN count(*)")
        .sql("SELECT count(*) FROM persons WHERE name >= 'K' AND name < 'L'");
const SINCE_RANGE: Texts =
    Texts::graph("MATCH (:Person)-[k:KNOWS]->(:Person) WHERE k.since >= 2020 RETURN count(*)")
        .sql("SELECT count(*) FROM knows WHERE since >= 2020");

const IDS_IN_PARAMETER: Texts =
    Texts::graph("MATCH (p:Person) WHERE p.id IN $ids RETURN p.id, p.name")
        .sql("SELECT id, name FROM persons WHERE id IN (SELECT value FROM json_each($ids))")
        .overriding(&[(
            EngineKind::Duckdb,
            "SELECT id, name FROM persons WHERE id IN (SELECT unnest(CAST($ids AS BIGINT[])))",
        )]);
const NAMES_IN_LIST: Texts = Texts::graph(
    "MATCH (p:Person) WHERE p.name IN ['Ada Lovelace', 'Alan Turing', 'Grace Hopper', 'Linus Torvalds', 'Barbara Liskov'] RETURN p.name, count(*)",
)
.sql(
    "SELECT name, count(*) FROM persons WHERE name IN ('Ada Lovelace', 'Alan Turing', 'Grace Hopper', 'Linus Torvalds', 'Barbara Liskov') GROUP BY name",
);
const AGES_IN_LIST: Texts =
    Texts::graph("MATCH (p:Person) WHERE p.age IN [21, 34, 55, 68] RETURN p.age, count(*)")
        .sql("SELECT age, count(*) FROM persons WHERE age IN (21, 34, 55, 68) GROUP BY age");

const PROBE_SINCE: Texts = Texts::graph(
    "MATCH (a:Person {id: $id})-[k:KNOWS]->(b:Person) WHERE k.since > 2010 RETURN b.id, k.since",
)
.sql("SELECT dst, since FROM knows WHERE src = $id AND since > 2010");
const ENDPOINT_COMPARE: Texts = Texts::graph(
    "MATCH (a:Person)-[k:KNOWS]->(b:Person) WHERE k.since < 2000 AND a.age > b.age RETURN count(*)",
)
.sql(
    "SELECT count(*) FROM knows k JOIN persons a ON a.id = k.src JOIN persons b ON b.id = k.dst WHERE k.since < 2000 AND a.age > b.age",
);
const HUB_SINCE: Texts = Texts::graph(
    "MATCH (:Person)-[k:KNOWS]->(h:Person {id: $hub}) WHERE k.since >= 2005 AND k.since <= 2010 RETURN count(*)",
)
.sql("SELECT count(*) FROM knows WHERE dst = $hub AND since >= 2005 AND since <= 2010");

const UNDIRECTED_1_HOP: Texts =
    Texts::graph("MATCH (a:Person {id: $id})-[:KNOWS]-(b:Person) RETURN count(DISTINCT b)")
        .sql(
            "SELECT count(DISTINCT x) FROM (SELECT dst AS x FROM knows WHERE src = $id UNION ALL SELECT src FROM knows WHERE dst = $id)",
        );
const UNDIRECTED_2_HOP: Texts = Texts::graph(
    "MATCH (a:Person {id: $id})-[:KNOWS]-(b:Person)-[:KNOWS]-(c:Person) WHERE c.id <> $id RETURN count(DISTINCT c)",
)
.sql(
    "WITH edges(x, y) AS (SELECT src, dst FROM knows UNION ALL SELECT dst, src FROM knows) SELECT count(DISTINCT e2.y) FROM edges e1 JOIN edges e2 ON e2.x = e1.y WHERE e1.x = $id AND e2.y <> $id",
);
const CO_FOLLOWERS: Texts = Texts::graph(
    "MATCH (a:Person {id: $id})-[:KNOWS]->(b:Person)<-[:KNOWS]-(c:Person) WHERE c.id <> $id RETURN count(DISTINCT c)",
)
.sql(
    "SELECT count(DISTINCT k2.src) FROM knows k1 JOIN knows k2 ON k2.dst = k1.dst WHERE k1.src = $id AND k2.src <> $id",
);
const MUTUAL: Texts =
    Texts::graph("MATCH (a:Person)-[:KNOWS]->(b:Person)-[:KNOWS]->(a) RETURN count(*)")
        .sql("SELECT count(*) FROM knows k1 JOIN knows k2 ON k2.src = k1.dst AND k2.dst = k1.src");

const TRIANGLES_FROM: Texts = Texts::graph(
    "MATCH (a:Person {id: $id})-[:KNOWS]->(b:Person)-[:KNOWS]->(c:Person)-[:KNOWS]->(a) RETURN count(*)",
)
.sql(
    "SELECT count(*) FROM knows k1 JOIN knows k2 ON k2.src = k1.dst JOIN knows k3 ON k3.src = k2.dst AND k3.dst = k1.src WHERE k1.src = $id",
);
const TRIANGLES_ALL: Texts = Texts::graph(
    "MATCH (a:Person)-[:KNOWS]->(b:Person)-[:KNOWS]->(c:Person)-[:KNOWS]->(a) RETURN count(*)",
)
.sql(
    "SELECT count(*) FROM knows k1 JOIN knows k2 ON k2.src = k1.dst JOIN knows k3 ON k3.src = k2.dst AND k3.dst = k1.src",
);
const RINGS_FROM: Texts = Texts::graph(
    "MATCH (a:Person {id: $id})-[:KNOWS]->(b:Person)-[:KNOWS]->(c:Person)-[:KNOWS]->(d:Person)-[:KNOWS]->(a) WHERE c.id <> a.id AND d.id <> b.id RETURN count(*)",
)
.sql(
    "SELECT count(*) FROM knows k1 JOIN knows k2 ON k2.src = k1.dst JOIN knows k3 ON k3.src = k2.dst JOIN knows k4 ON k4.src = k3.dst AND k4.dst = k1.src WHERE k1.src = $id AND k2.dst <> k1.src AND k3.dst <> k1.dst",
);
const UNDIRECTED_TRIANGLES: Texts = Texts::graph(
    "MATCH (a:Person {id: $id})-[:KNOWS]-(b:Person)-[:KNOWS]-(c:Person)-[:KNOWS]-(a) WHERE b.id < c.id RETURN count(*)",
)
.sql(
    "WITH edges(x, y) AS (SELECT src, dst FROM knows UNION ALL SELECT dst, src FROM knows) SELECT count(*) FROM edges e1 JOIN edges e2 ON e2.x = e1.y JOIN edges e3 ON e3.x = e2.y AND e3.y = e1.x WHERE e1.x = $id AND e1.y < e2.y",
);

const DISTINCT_UNDIRECTED_PAIRS: Texts = Texts::graph(
    "MATCH (a:Person {id: $id})-[:KNOWS]-(b:Person)-[:KNOWS]-(c:Person) WHERE c.id <> $id RETURN DISTINCT b.id, c.id",
)
.sql(
    "WITH edges(x, y) AS (SELECT src, dst FROM knows UNION ALL SELECT dst, src FROM knows) SELECT DISTINCT e1.y, e2.y FROM edges e1 JOIN edges e2 ON e2.x = e1.y WHERE e1.x = $id AND e2.y <> $id",
);
const DISTINCT_FOF_CITIES: Texts = Texts::graph(
    "MATCH (a:Person {id: $id})-[:KNOWS]->(:Person)-[:KNOWS]->(:Person)-[:LIVES_IN]->(x:City) RETURN DISTINCT x.id",
)
.sql(
    "SELECT DISTINCT l.dst FROM knows k1 JOIN knows k2 ON k2.src = k1.dst JOIN lives_in l ON l.src = k2.dst WHERE k1.src = $id",
);

const TWO_ENDPOINTS: Texts = Texts::graph(
    "MATCH (a:Person {id: $a}), (b:Person {id: $b}) OPTIONAL MATCH (a)-[k:KNOWS]->(b) RETURN a.name, b.name, k.since",
)
.sql(
    "SELECT a.name, b.name, k.since FROM persons a JOIN persons b ON b.id = $b LEFT JOIN knows k ON k.src = a.id AND k.dst = b.id WHERE a.id = $a",
);
const MANY_PARAMETERS: Texts = Texts::graph(
    "MATCH (p:Person) WHERE p.id IN [$p1, $p2, $p3, $p4, $p5, $p6, $p7, $p8] AND p.age >= $min RETURN p.id, p.name",
)
.sql(
    "SELECT id, name FROM persons WHERE id IN ($p1, $p2, $p3, $p4, $p5, $p6, $p7, $p8) AND age >= $min",
);
const PROFILE_CARD: Texts = Texts::graph(
    "MATCH (p:Person {id: $id}) OPTIONAL MATCH (p)-[:LIVES_IN]->(c:City) RETURN p.name, p.age, c.name, COUNT { MATCH (p)-[:KNOWS]->(:Person) }, COUNT { MATCH (p)<-[:KNOWS]-(:Person) }",
)
.sql(
    "SELECT p.name, p.age, c.name, (SELECT count(*) FROM knows WHERE src = p.id), (SELECT count(*) FROM knows WHERE dst = p.id) FROM persons p LEFT JOIN lives_in l ON l.src = p.id LEFT JOIN cities c ON c.id = l.dst WHERE p.id = $id",
);

pub fn workloads() -> Vec<Workload> {
    vec![
        read("filter/2-hop-every-hop", two_hop_filtered),
        read("filter/3-hop-every-hop", three_hop_filtered),
        read("optional/chain", optional_chain),
        read("optional/scan", optional_scan),
        read("optional/friends", optional_friends),
        read("aggregate/in-degree-groups", in_degree_groups),
        read("aggregate/name-groups", name_groups),
        read("aggregate/city-since-groups", city_since_groups),
        read("aggregate/friend-age-groups", friend_age_groups),
        read("order/computed", order_computed),
        read("order/degree", order_degree),
        read("order/string-length", order_length),
        read("paginate/skip-deep", skip_deep),
        read("paginate/pages-by-name", pages_by_name),
        read("paginate/hub-followers", hub_followers_page),
        read("topk/per-city", top_per_city),
        read("topk/latest-friends", latest_friends),
        read("topk/oldest-per-city", oldest_per_city),
        read("collect/unwind-friends", collect_unwind_friends),
        read("collect/unwind-groups", collect_unwind_groups),
        read("collect/list-sizes", collect_sizes),
        read("subquery/exists", exists_old_friend),
        read("subquery/not-exists", not_exists_city),
        read("subquery/count-filter", count_filter),
        read("subquery/count-project", count_project),
        read("subquery/recommend", recommend),
        read("string/starts-with", starts_with),
        read("string/ends-with", ends_with),
        read("string/lower-contains", lower_contains),
        read("string/initials", initials),
        read("string/first-names", first_names),
        read("string/concat-distinct", concat_distinct),
        read("regex/name", regex_name),
        read("regex/bio", regex_bio),
        read("range/id", id_range),
        read("range/age", age_range),
        read("range/name", name_range),
        read("range/since", since_range),
        read("in-list/ids-parameter", ids_in_parameter),
        read("in-list/names", names_in_list),
        read("in-list/ages", ages_in_list),
        read("rel-filter/probe-since", probe_since),
        read("rel-filter/endpoint-compare", endpoint_compare),
        read("rel-filter/hub-since", hub_since),
        read("undirected/1-hop", undirected_1_hop),
        read("undirected/2-hop", undirected_2_hop),
        read("bidirectional/co-followers", co_followers),
        read("bidirectional/mutual", mutual),
        read("cycle/triangles-from", triangles_from),
        read("cycle/triangles-all", triangles_all),
        read("cycle/rings-from", rings_from),
        read("cycle/undirected-triangles", undirected_triangles),
        read("distinct/undirected-pairs", distinct_undirected_pairs),
        read("distinct/fof-cities", distinct_fof_cities),
        read("point/two-endpoints", two_endpoints),
        read("point/many-parameters", many_parameters),
        read("point/profile-card", profile_card),
    ]
}

fn read(name: &'static str, run: fn(&Context<'_>) -> Result<Measurement, EngineError>) -> Workload {
    Workload {
        name,
        needs: Needs::Base,
        comparison: Comparison::Exact,
        file_only: false,
        run,
    }
}

fn single(ctx: &Context<'_>, texts: &Texts) -> Result<Measurement, EngineError> {
    run_queries(ctx, &[Query::new(ctx.text(texts)?)])
}

fn each(
    ctx: &Context<'_>,
    texts: &Texts,
    count: usize,
    query: impl Fn(Query<'static>, usize) -> Query<'static>,
) -> Result<Measurement, EngineError> {
    let text = ctx.text(texts)?;
    let queries: Vec<Query<'static>> = (0..count)
        .map(|step| query(Query::new(text), step))
        .collect();
    run_queries(ctx, &queries)
}

fn probe(ctx: &Context<'_>, step: usize) -> i64 {
    ctx.data.probes.get(step).copied().unwrap_or(HUB)
}

fn per_traversal(ctx: &Context<'_>, texts: &Texts) -> Result<Measurement, EngineError> {
    each(ctx, texts, ctx.plan.traversals, |query, step| {
        query.int("id", probe(ctx, step))
    })
}

fn per_lookup(ctx: &Context<'_>, texts: &Texts) -> Result<Measurement, EngineError> {
    each(ctx, texts, ctx.plan.lookups, |query, step| {
        query.int("id", probe(ctx, step))
    })
}

fn persons(ctx: &Context<'_>) -> i64 {
    ctx.data.people.len() as i64
}

fn hub_followers(ctx: &Context<'_>) -> i64 {
    ctx.data.knows.iter().filter(|edge| edge.to == HUB).count() as i64
}

fn two_hop_filtered(ctx: &Context<'_>) -> Result<Measurement, EngineError> {
    per_traversal(ctx, &TWO_HOP_FILTERED)
}

fn three_hop_filtered(ctx: &Context<'_>) -> Result<Measurement, EngineError> {
    per_traversal(ctx, &THREE_HOP_FILTERED)
}

fn optional_chain(ctx: &Context<'_>) -> Result<Measurement, EngineError> {
    per_lookup(ctx, &OPTIONAL_CHAIN)
}

fn optional_scan(ctx: &Context<'_>) -> Result<Measurement, EngineError> {
    single(ctx, &OPTIONAL_SCAN)
}

fn optional_friends(ctx: &Context<'_>) -> Result<Measurement, EngineError> {
    per_traversal(ctx, &OPTIONAL_FRIENDS)
}

fn in_degree_groups(ctx: &Context<'_>) -> Result<Measurement, EngineError> {
    single(ctx, &IN_DEGREE_GROUPS)
}

fn name_groups(ctx: &Context<'_>) -> Result<Measurement, EngineError> {
    single(ctx, &NAME_GROUPS)
}

fn city_since_groups(ctx: &Context<'_>) -> Result<Measurement, EngineError> {
    single(ctx, &CITY_SINCE_GROUPS)
}

fn friend_age_groups(ctx: &Context<'_>) -> Result<Measurement, EngineError> {
    single(ctx, &FRIEND_AGE_GROUPS)
}

fn order_computed(ctx: &Context<'_>) -> Result<Measurement, EngineError> {
    single(ctx, &ORDER_COMPUTED)
}

fn order_degree(ctx: &Context<'_>) -> Result<Measurement, EngineError> {
    single(ctx, &ORDER_DEGREE)
}

fn order_length(ctx: &Context<'_>) -> Result<Measurement, EngineError> {
    single(ctx, &ORDER_LENGTH)
}

fn skip_deep(ctx: &Context<'_>) -> Result<Measurement, EngineError> {
    each(ctx, &SKIP_DEEP, 1, |query, _| {
        query.int("skip", persons(ctx) * 3 / 4)
    })
}

fn pages_by_name(ctx: &Context<'_>) -> Result<Measurement, EngineError> {
    let stride = (persons(ctx) / PAGES as i64).max(PAGE);
    each(ctx, &PAGES_BY_NAME, PAGES, |query, step| {
        query.int("skip", step as i64 * stride)
    })
}

fn hub_followers_page(ctx: &Context<'_>) -> Result<Measurement, EngineError> {
    let followers = hub_followers(ctx);
    each(ctx, &HUB_FOLLOWERS_PAGE, PAGES, |query, step| {
        query
            .int("hub", HUB)
            .int("skip", followers * step as i64 / PAGES as i64)
    })
}

fn top_per_city(ctx: &Context<'_>) -> Result<Measurement, EngineError> {
    single(ctx, &TOP_PER_CITY)
}

fn latest_friends(ctx: &Context<'_>) -> Result<Measurement, EngineError> {
    each(ctx, &LATEST_FRIENDS, 1, |query, _| {
        query.int("last", persons(ctx) / 10)
    })
}

fn oldest_per_city(ctx: &Context<'_>) -> Result<Measurement, EngineError> {
    single(ctx, &OLDEST_PER_CITY)
}

fn collect_unwind_friends(ctx: &Context<'_>) -> Result<Measurement, EngineError> {
    per_traversal(ctx, &COLLECT_UNWIND_FRIENDS)
}

fn collect_unwind_groups(ctx: &Context<'_>) -> Result<Measurement, EngineError> {
    single(ctx, &COLLECT_UNWIND_GROUPS)
}

fn collect_sizes(ctx: &Context<'_>) -> Result<Measurement, EngineError> {
    single(ctx, &COLLECT_SIZES)
}

fn exists_old_friend(ctx: &Context<'_>) -> Result<Measurement, EngineError> {
    single(ctx, &EXISTS_OLD_FRIEND)
}

fn not_exists_city(ctx: &Context<'_>) -> Result<Measurement, EngineError> {
    single(ctx, &NOT_EXISTS_CITY)
}

fn count_filter(ctx: &Context<'_>) -> Result<Measurement, EngineError> {
    single(ctx, &COUNT_FILTER)
}

fn count_project(ctx: &Context<'_>) -> Result<Measurement, EngineError> {
    per_lookup(ctx, &COUNT_PROJECT)
}

fn recommend(ctx: &Context<'_>) -> Result<Measurement, EngineError> {
    per_traversal(ctx, &RECOMMEND)
}

fn starts_with(ctx: &Context<'_>) -> Result<Measurement, EngineError> {
    single(ctx, &STARTS_WITH)
}

fn ends_with(ctx: &Context<'_>) -> Result<Measurement, EngineError> {
    single(ctx, &ENDS_WITH)
}

fn lower_contains(ctx: &Context<'_>) -> Result<Measurement, EngineError> {
    single(ctx, &LOWER_CONTAINS)
}

fn initials(ctx: &Context<'_>) -> Result<Measurement, EngineError> {
    single(ctx, &INITIALS)
}

fn first_names(ctx: &Context<'_>) -> Result<Measurement, EngineError> {
    single(ctx, &FIRST_NAMES)
}

fn concat_distinct(ctx: &Context<'_>) -> Result<Measurement, EngineError> {
    single(ctx, &CONCAT_DISTINCT)
}

fn regex_name(ctx: &Context<'_>) -> Result<Measurement, EngineError> {
    single(ctx, &REGEX_NAME)
}

fn regex_bio(ctx: &Context<'_>) -> Result<Measurement, EngineError> {
    single(ctx, &REGEX_BIO)
}

fn id_range(ctx: &Context<'_>) -> Result<Measurement, EngineError> {
    each(ctx, &ID_RANGE_SCAN, ctx.plan.lookups, |query, step| {
        let low = probe(ctx, step);
        query.int("low", low).int("high", low + ID_RANGE)
    })
}

fn age_range(ctx: &Context<'_>) -> Result<Measurement, EngineError> {
    single(ctx, &AGE_RANGE)
}

fn name_range(ctx: &Context<'_>) -> Result<Measurement, EngineError> {
    single(ctx, &NAME_RANGE)
}

fn since_range(ctx: &Context<'_>) -> Result<Measurement, EngineError> {
    single(ctx, &SINCE_RANGE)
}

fn ids_in_parameter(ctx: &Context<'_>) -> Result<Measurement, EngineError> {
    each(ctx, &IDS_IN_PARAMETER, ctx.plan.lookups, |query, step| {
        let ids: Vec<i64> = (0..IDS_PER_QUERY)
            .map(|offset| {
                probe(
                    ctx,
                    (step * IDS_PER_QUERY + offset) % ctx.data.probes.len().max(1),
                )
            })
            .collect();
        query.ints("ids", &ids)
    })
}

fn names_in_list(ctx: &Context<'_>) -> Result<Measurement, EngineError> {
    single(ctx, &NAMES_IN_LIST)
}

fn ages_in_list(ctx: &Context<'_>) -> Result<Measurement, EngineError> {
    single(ctx, &AGES_IN_LIST)
}

fn probe_since(ctx: &Context<'_>) -> Result<Measurement, EngineError> {
    per_lookup(ctx, &PROBE_SINCE)
}

fn endpoint_compare(ctx: &Context<'_>) -> Result<Measurement, EngineError> {
    single(ctx, &ENDPOINT_COMPARE)
}

fn hub_since(ctx: &Context<'_>) -> Result<Measurement, EngineError> {
    each(ctx, &HUB_SINCE, 1, |query, _| query.int("hub", HUB))
}

fn undirected_1_hop(ctx: &Context<'_>) -> Result<Measurement, EngineError> {
    per_lookup(ctx, &UNDIRECTED_1_HOP)
}

fn undirected_2_hop(ctx: &Context<'_>) -> Result<Measurement, EngineError> {
    per_traversal(ctx, &UNDIRECTED_2_HOP)
}

fn co_followers(ctx: &Context<'_>) -> Result<Measurement, EngineError> {
    per_traversal(ctx, &CO_FOLLOWERS)
}

fn mutual(ctx: &Context<'_>) -> Result<Measurement, EngineError> {
    single(ctx, &MUTUAL)
}

fn triangles_from(ctx: &Context<'_>) -> Result<Measurement, EngineError> {
    per_lookup(ctx, &TRIANGLES_FROM)
}

fn triangles_all(ctx: &Context<'_>) -> Result<Measurement, EngineError> {
    single(ctx, &TRIANGLES_ALL)
}

fn rings_from(ctx: &Context<'_>) -> Result<Measurement, EngineError> {
    per_traversal(ctx, &RINGS_FROM)
}

fn undirected_triangles(ctx: &Context<'_>) -> Result<Measurement, EngineError> {
    per_traversal(ctx, &UNDIRECTED_TRIANGLES)
}

fn distinct_undirected_pairs(ctx: &Context<'_>) -> Result<Measurement, EngineError> {
    per_traversal(ctx, &DISTINCT_UNDIRECTED_PAIRS)
}

fn distinct_fof_cities(ctx: &Context<'_>) -> Result<Measurement, EngineError> {
    per_traversal(ctx, &DISTINCT_FOF_CITIES)
}

fn two_endpoints(ctx: &Context<'_>) -> Result<Measurement, EngineError> {
    let knows = &ctx.data.knows;
    each(ctx, &TWO_ENDPOINTS, ctx.plan.lookups, |query, step| {
        let (a, b) = match knows.get(step * 7) {
            Some(edge) if step % 2 == 0 => (edge.from, edge.to),
            _ => (probe(ctx, step), probe(ctx, step + 1)),
        };
        query.int("a", a).int("b", b)
    })
}

fn many_parameters(ctx: &Context<'_>) -> Result<Measurement, EngineError> {
    const NAMES: [&str; 8] = ["p1", "p2", "p3", "p4", "p5", "p6", "p7", "p8"];
    each(ctx, &MANY_PARAMETERS, ctx.plan.lookups, |query, step| {
        NAMES
            .iter()
            .enumerate()
            .fold(query.int("min", 30), |query, (offset, name)| {
                query.int(name, probe(ctx, step * NAMES.len() + offset))
            })
    })
}

fn profile_card(ctx: &Context<'_>) -> Result<Measurement, EngineError> {
    per_lookup(ctx, &PROFILE_CARD)
}
