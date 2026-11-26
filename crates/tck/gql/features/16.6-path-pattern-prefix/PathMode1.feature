#encoding: utf-8

Feature: PathMode1 - ISO/IEC 39075:2024 16.6 <path pattern prefix>, <path mode>

  Scenario: [1] WALK may take an edge twice
    Given an empty graph
    And having executed:
      """
      INSERT (:N {k: 1})-[:R]->(:N {k: 2})
      """
    When executing query:
      """
      MATCH p = ANY SHORTEST WALK (x:N {k: 1})-[:R]-+(x)
      RETURN [n IN nodes(p) | n.k] AS ks
      """
    Then the result should be, in any order:
      | ks        |
      | [1, 2, 1] |
    And no side effects

  Scenario: [2] TRAIL takes no edge twice
    Given an empty graph
    And having executed:
      """
      INSERT (a:N {k: 1})-[:R]->(:N {k: 2})-[:R]->(c:N {k: 3})-[:R]->(a)
      """
    When executing query:
      """
      MATCH p = ANY SHORTEST TRAIL (x:N {k: 1})-[:R]-+(x)
      RETURN length(p) AS length
      """
    Then the result should be, in any order:
      | length |
      | 3      |
    And no side effects

  Scenario: [3] ACYCLIC visits no node twice
    Given an empty graph
    And having executed:
      """
      INSERT (a:N {k: 1})-[:R]->(:N {k: 2})-[:R]->(c:N {k: 3})-[:R]->(a)
      """
    When executing query:
      """
      MATCH p = ACYCLIC (x:N {k: 1})-[:R]->+(y)
      RETURN y.k AS k
      ORDER BY k
      """
    Then the result should be, in order:
      | k |
      | 2 |
      | 3 |
    And no side effects

  Scenario: [4] SIMPLE visits no node twice except that a path may end where it starts
    Given an empty graph
    And having executed:
      """
      INSERT (a:N {k: 1})-[:R]->(:N {k: 2})-[:R]->(c:N {k: 3})-[:R]->(a)
      """
    When executing query:
      """
      MATCH p = SIMPLE (x:N {k: 1})-[:R]->+(y)
      RETURN y.k AS k
      ORDER BY k
      """
    Then the result should be, in order:
      | k |
      | 1 |
      | 2 |
      | 3 |
    And no side effects

  Scenario: [5] TRAIL without a selector gives every trail
    Given an empty graph
    And having executed:
      """
      INSERT (a:N {k: 1})-[:R]->(b:N {k: 2}), (b)-[:R]->(a), (b)-[:R]->(a)
      """
    When executing query:
      """
      MATCH p = TRAIL (x:N {k: 1})-[:R]-+(y)
      RETURN count(*) AS paths
      """
    Then the result should be, in any order:
      | paths |
      | 15    |
    And no side effects

  Scenario: [6] A WALK with an unbounded quantifier needs a selector
    Given an empty graph
    When executing query:
      """
      MATCH p = WALK (x)-[:R]->+(y)
      RETURN p
      """
    Then a SyntaxError should be raised at compile time: InvalidPathPattern

  Scenario: [7] A selector without a path mode searches walks
    Given an empty graph
    And having executed:
      """
      INSERT (:N {k: 1})-[:R]->(:N {k: 2})
      """
    When executing query:
      """
      MATCH p = ANY SHORTEST (x:N {k: 1})-[:R]-+(x)
      RETURN length(p) AS length
      """
    Then the result should be, in any order:
      | length |
      | 2      |
    And no side effects

  Scenario: [8] A WALK with a bounded quantifier needs no selector
    Given an empty graph
    And having executed:
      """
      INSERT (:N {k: 1})-[:R]->(:N {k: 2})
      """
    When executing query:
      """
      MATCH p = WALK (x:N {k: 1})-[:R]-{2}(y)
      RETURN y.k AS k
      """
    Then the result should be, in any order:
      | k |
      | 1 |
    And no side effects
