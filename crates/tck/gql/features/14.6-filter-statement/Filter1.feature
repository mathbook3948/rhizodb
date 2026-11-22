#encoding: utf-8

Feature: Filter1 - ISO/IEC 39075:2024 14.6 <filter statement>

  Scenario: [1] FILTER keeps the rows whose condition is true
    Given an empty graph
    When executing query:
      """
      UNWIND [1, 2, 3] AS k
      FILTER k > 1
      RETURN k
      """
    Then the result should be, in any order:
      | k |
      | 2 |
      | 3 |
    And no side effects

  Scenario: [2] FILTER takes an optional WHERE
    Given an empty graph
    When executing query:
      """
      UNWIND [1, 2, 3] AS k
      FILTER WHERE k < 3
      RETURN k
      """
    Then the result should be, in any order:
      | k |
      | 1 |
      | 2 |
    And no side effects
