#encoding: utf-8

Feature: Quantifier1 - ISO/IEC 39075:2024 16.11 <graph pattern quantifier>

  Background:
    Given an empty graph
    And having executed:
      """
      INSERT (:N {k: 1})-[:R {w: 1}]->(:N {k: 2})-[:R {w: 2}]->(:N {k: 3})-[:R {w: 3}]->(:N {k: 4})
      """

  Scenario: [1] A quantified edge repeats between its bounds
    When executing query:
      """
      MATCH (x:N {k: 1})-[:R]->{2,3}(y)
      RETURN y.k AS k
      ORDER BY k
      """
    Then the result should be, in order:
      | k |
      | 3 |
      | 4 |
    And no side effects

  Scenario: [2] A fixed quantifier repeats exactly that often
    When executing query:
      """
      MATCH (x:N {k: 1})-[:R]->{2}(y)
      RETURN y.k AS k
      """
    Then the result should be, in any order:
      | k |
      | 3 |
    And no side effects

  Scenario: [3] An asterisk allows zero repetitions and a plus sign at least one
    When executing query:
      """
      MATCH (x:N {k: 3})-[:R]->*(y)
      WITH collect(y.k) AS star
      MATCH (x:N {k: 3})-[:R]->+(y)
      RETURN star, collect(y.k) AS plus
      """
    Then the result should be, in any order:
      | star   | plus |
      | [3, 4] | [4]  |
    And no side effects

  Scenario: [4] A quantified path pattern binds each variable to a list of its iterations
    When executing query:
      """
      MATCH (x:N {k: 1})((a)-[r:R]->(b)){3}(y)
      RETURN [n IN a | n.k] AS starts, [e IN r | e.w] AS weights, [n IN b | n.k] AS ends, y.k AS last
      """
    Then the result should be, in any order:
      | starts    | weights   | ends      | last |
      | [1, 2, 3] | [1, 2, 3] | [2, 3, 4] | 4    |
    And no side effects

  Scenario: [5] A WHERE inside a quantified path pattern tests each iteration
    When executing query:
      """
      MATCH (x:N {k: 1})((a)-[r:R]->(b) WHERE r.w < 3)+(y)
      RETURN y.k AS k
      ORDER BY k
      """
    Then the result should be, in order:
      | k |
      | 2 |
      | 3 |
    And no side effects

  Scenario: [6] A quantified path pattern sits between other edges
    When executing query:
      """
      MATCH (x:N {k: 1})-[:R]->(m)((a)-[:R]->(b)){1,2}(y)-[:R]->(z)
      RETURN m.k AS m, y.k AS y, z.k AS z
      """
    Then the result should be, in any order:
      | m | y | z |
      | 2 | 3 | 4 |
    And no side effects

  Scenario: [7] Nodes of a quantified path pattern join the nodes around it
    When executing query:
      """
      MATCH (x:N {k: 2})((a:N)-[:R]->(b:N)){2}(y)
      RETURN x.k AS x, [n IN a | n.k] AS a, y.k AS y
      """
    Then the result should be, in any order:
      | x | a      | y |
      | 2 | [2, 3] | 4 |
    And no side effects

  Scenario: [8] An iteration takes several edges
    When executing query:
      """
      MATCH (x:N {k: 1})((a)-[:R]->(m)-[:R]->(b)){1,2}(y)
      RETURN [n IN m | n.k] AS middles, y.k AS y
      """
    Then the result should be, in any order:
      | middles | y |
      | [2]     | 3 |
    And no side effects

  Scenario: [9] A selector applies to a quantified path pattern
    When executing query:
      """
      MATCH p = ANY SHORTEST (x:N {k: 1})((a)-[r:R]->(b) WHERE r.w > 0)+(y:N {k: 4})
      RETURN length(p) AS length, [n IN b | n.k] AS ends
      """
    Then the result should be, in any order:
      | length | ends      |
      | 3      | [2, 3, 4] |
    And no side effects

  Scenario: [10] A quantified path pattern takes no relationship of the same match twice
    When executing query:
      """
      MATCH (x:N {k: 1})((a)-[:R]->(b))+(y), (u)-[s:R]->(v)
      RETURN y.k AS y, v.k AS v
      ORDER BY y, v
      """
    Then the result should be, in order:
      | y | v |
      | 2 | 3 |
      | 2 | 4 |
      | 3 | 4 |
    And no side effects

  Scenario: [11] A group variable cannot be bound before the quantified path pattern
    When executing query:
      """
      MATCH (a)
      MATCH ((a)-[:R]->(b))+
      RETURN a
      """
    Then a SyntaxError should be raised at compile time: VariableAlreadyBound

  Scenario: [12] A quantified path pattern needs an edge
    When executing query:
      """
      MATCH ((c)){1,2}
      RETURN c
      """
    Then a SyntaxError should be raised at compile time: InvalidPathPattern
