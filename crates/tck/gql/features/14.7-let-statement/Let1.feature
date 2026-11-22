#encoding: utf-8

Feature: Let1 - ISO/IEC 39075:2024 14.7 <let statement>

  Scenario: [1] LET binds a value to a new variable
    Given an empty graph
    When executing query:
      """
      UNWIND [1, 2] AS k
      LET d = k * 2
      RETURN k, d
      """
    Then the result should be, in any order:
      | k | d |
      | 1 | 2 |
      | 2 | 4 |
    And no side effects
