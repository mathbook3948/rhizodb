#encoding: utf-8

Feature: Offset1 - ISO/IEC 39075:2024 16.19 <offset clause>

  Scenario: [1] OFFSET skips the first rows of an ordered result
    Given an empty graph
    And having executed:
      """
      INSERT ({k: 1}), ({k: 2}), ({k: 3}), ({k: 4})
      """
    When executing query:
      """
      MATCH (n)
      RETURN n.k AS k
      ORDER BY k
      OFFSET 2
      """
    Then the result should be, in order:
      | k |
      | 3 |
      | 4 |
    And no side effects

  Scenario: [2] OFFSET before LIMIT pages through a result
    Given an empty graph
    When executing query:
      """
      UNWIND [1, 2, 3, 4, 5] AS k
      RETURN k
      ORDER BY k
      OFFSET 1
      LIMIT 2
      """
    Then the result should be, in order:
      | k |
      | 2 |
      | 3 |
    And no side effects

  Scenario: [3] OFFSET takes a parameter
    Given an empty graph
    And parameters are:
      | o | 3 |
    When executing query:
      """
      UNWIND [1, 2, 3, 4] AS k
      RETURN k
      ORDER BY k
      OFFSET $o
      """
    Then the result should be, in order:
      | k |
      | 4 |
    And no side effects

  Scenario: [4] OFFSET past the end gives no rows
    Given an empty graph
    When executing query:
      """
      UNWIND [1, 2] AS k
      RETURN k
      OFFSET 5
      """
    Then the result should be, in any order:
      | k |
    And no side effects

  Scenario: [5] OFFSET zero keeps every row
    Given an empty graph
    When executing query:
      """
      UNWIND [1, 2] AS k
      RETURN k
      OFFSET 0
      """
    Then the result should be, in any order:
      | k |
      | 1 |
      | 2 |
    And no side effects

  Scenario: [6] A negative OFFSET fails
    Given an empty graph
    When executing query:
      """
      UNWIND [1, 2] AS k
      RETURN k
      OFFSET -1
      """
    Then a SyntaxError should be raised at compile time: NegativeIntegerArgument

  Scenario: [7] A float OFFSET fails
    Given an empty graph
    When executing query:
      """
      UNWIND [1, 2] AS k
      RETURN k
      OFFSET 1.5
      """
    Then a SyntaxError should be raised at compile time: InvalidArgumentType
