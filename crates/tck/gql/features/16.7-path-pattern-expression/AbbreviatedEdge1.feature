#encoding: utf-8

Feature: AbbreviatedEdge1 - ISO/IEC 39075:2024 16.7 <path pattern expression>, <abbreviated edge pattern>

  Scenario: [1] An edge pointing right matches outgoing edges only
    Given an empty graph
    And having executed:
      """
      INSERT (:A {k: 'a'})-[:T]->(:B {k: 'b'})
      """
    When executing query:
      """
      MATCH (x)->(y)
      RETURN x.k, y.k
      """
    Then the result should be, in any order:
      | x.k | y.k |
      | 'a' | 'b' |
    And no side effects

  Scenario: [2] An edge pointing left matches incoming edges only
    Given an empty graph
    And having executed:
      """
      INSERT (:A {k: 'a'})-[:T]->(:B {k: 'b'})
      """
    When executing query:
      """
      MATCH (x)<-(y)
      RETURN x.k, y.k
      """
    Then the result should be, in any order:
      | x.k | y.k |
      | 'b' | 'a' |
    And no side effects

  Scenario: [3] An edge in any direction matches both orientations
    Given an empty graph
    And having executed:
      """
      INSERT (:A {k: 'a'})-[:T]->(:B {k: 'b'})
      """
    When executing query:
      """
      MATCH (x)-(y)
      RETURN x.k, y.k
      """
    Then the result should be, in any order:
      | x.k | y.k |
      | 'a' | 'b' |
      | 'b' | 'a' |
    And no side effects

  Scenario: [4] Abbreviated edges chain into a longer path
    Given an empty graph
    And having executed:
      """
      INSERT (:N {k: 1})-[:T]->(:N {k: 2})-[:T]->(:N {k: 3})
      """
    When executing query:
      """
      MATCH (a)->(b)->(c)
      RETURN a.k, b.k, c.k
      """
    Then the result should be, in any order:
      | a.k | b.k | c.k |
      | 1   | 2   | 3   |
    And no side effects

  Scenario: [5] Abbreviated and full edge patterns mix in one path
    Given an empty graph
    And having executed:
      """
      INSERT (:N {k: 1})-[:T]->(:N {k: 2})-[:U]->(:N {k: 3})
      """
    When executing query:
      """
      MATCH (a)->(b)-[:U]->(c)
      RETURN a.k, b.k, c.k
      """
    Then the result should be, in any order:
      | a.k | b.k | c.k |
      | 1   | 2   | 3   |
    And no side effects

  Scenario: [6] An abbreviated edge works as a pattern predicate
    Given an empty graph
    And having executed:
      """
      INSERT (:N {k: 1})-[:T]->(:N {k: 2}), (:N {k: 3})
      """
    When executing query:
      """
      MATCH (n)
      WHERE (n)->()
      RETURN n.k
      """
    Then the result should be, in any order:
      | n.k |
      | 1   |
    And no side effects

  Scenario: [7] An abbreviated edge works in OPTIONAL MATCH
    Given an empty graph
    And having executed:
      """
      INSERT (:N {k: 1})-[:T]->(:N {k: 2})
      """
    When executing query:
      """
      MATCH (n)
      OPTIONAL MATCH (n)->(m)
      RETURN n.k, m.k
      """
    Then the result should be, in any order:
      | n.k | m.k  |
      | 1   | 2    |
      | 2   | null |
    And no side effects

  Scenario: [8] A minus sign between parenthesized terms in an expression is subtraction
    Given an empty graph
    When executing query:
      """
      WITH 5 AS a, 3 AS b
      RETURN (a)-(b) AS d
      """
    Then the result should be, in any order:
      | d |
      | 2 |
    And no side effects

  Scenario: [9] An edge pointing left or right matches both orientations
    Given an empty graph
    And having executed:
      """
      INSERT (:A {k: 'a'})-[:T]->(:B {k: 'b'})
      """
    When executing query:
      """
      MATCH (x)<->(y)
      RETURN x.k, y.k
      """
    Then the result should be, in any order:
      | x.k | y.k |
      | 'a' | 'b' |
      | 'b' | 'a' |
    And no side effects

  Scenario: [10] An undirected edge does not match a directed edge
    Given an empty graph
    And having executed:
      """
      INSERT (:A {k: 'a'})-[:T]->(:B {k: 'b'})
      """
    When executing query:
      """
      MATCH (x)~(y)
      RETURN x.k, y.k
      """
    Then the result should be, in any order:
      | x.k | y.k |
    And no side effects
