#encoding: utf-8

Feature: LabelExpression1 - ISO/IEC 39075:2024 16.8 <label expression>

  Scenario: [1] IS introduces a label expression in a node pattern
    Given an empty graph
    And having executed:
      """
      INSERT (:A {k: 1}), (:B {k: 2})
      """
    When executing query:
      """
      MATCH (n IS A)
      RETURN n.k
      """
    Then the result should be, in any order:
      | n.k |
      | 1   |
    And no side effects

  Scenario: [2] A label conjunction needs every label
    Given an empty graph
    And having executed:
      """
      INSERT (:A:B {k: 1}), (:A {k: 2})
      """
    When executing query:
      """
      MATCH (n:A&B)
      RETURN n.k
      """
    Then the result should be, in any order:
      | n.k |
      | 1   |
    And no side effects

  Scenario: [3] A label disjunction in a node pattern needs any label
    Given an empty graph
    And having executed:
      """
      INSERT (:A {k: 1}), (:B {k: 2}), (:C {k: 3})
      """
    When executing query:
      """
      MATCH (n:A|B)
      RETURN n.k
      """
    Then the result should be, in any order:
      | n.k |
      | 1   |
      | 2   |
    And no side effects

  Scenario: [4] A label negation excludes a label
    Given an empty graph
    And having executed:
      """
      INSERT (:A {k: 1}), (:B {k: 2})
      """
    When executing query:
      """
      MATCH (n:!A)
      RETURN n.k
      """
    Then the result should be, in any order:
      | n.k |
      | 2   |
    And no side effects

  Scenario: [5] A label disjunction in an edge pattern matches either type
    Given an empty graph
    And having executed:
      """
      INSERT (:N)-[:T {k: 1}]->(:N), (:N)-[:U {k: 2}]->(:N), (:N)-[:V {k: 3}]->(:N)
      """
    When executing query:
      """
      MATCH ()-[r:T|U]->()
      RETURN r.k
      """
    Then the result should be, in any order:
      | r.k |
      | 1   |
      | 2   |
    And no side effects
