#encoding: utf-8

Feature: LabeledPredicate1 - ISO/IEC 39075:2024 19.9 <labeled predicate>

  Scenario: [1] A label test on an edge is true for its type
    Given an empty graph
    And having executed:
      """
      INSERT (:A)-[:T]->(:B), (:A)-[:U]->(:B)
      """
    When executing query:
      """
      MATCH ()-[r]->()
      RETURN type(r) AS t, r:T AS isT
      """
    Then the result should be, in any order:
      | t   | isT   |
      | 'T' | true  |
      | 'U' | false |
    And no side effects

  Scenario: [2] A label test on an edge filters in WHERE
    Given an empty graph
    And having executed:
      """
      INSERT (:A)-[:T {k: 1}]->(:B), (:A)-[:U {k: 2}]->(:B)
      """
    When executing query:
      """
      MATCH ()-[r]->()
      WHERE r:U
      RETURN r.k
      """
    Then the result should be, in any order:
      | r.k |
      | 2   |
    And no side effects

  Scenario: [3] IS LABELED tests a node label
    Given an empty graph
    And having executed:
      """
      INSERT (:A {k: 1}), (:B {k: 2})
      """
    When executing query:
      """
      MATCH (n)
      WHERE n IS LABELED A
      RETURN n.k
      """
    Then the result should be, in any order:
      | n.k |
      | 1   |
    And no side effects
