#encoding: utf-8

Feature: Insert1 - ISO/IEC 39075:2024 13.2 <insert statement>

  Scenario: [1] Insert a node with a label and a property
    Given an empty graph
    When executing query:
      """
      INSERT (:Person {name: 'Ada'})
      """
    Then the result should be empty
    And the side effects should be:
      | +nodes      | 1 |
      | +labels     | 1 |
      | +properties | 1 |
    When executing control query:
      """
      MATCH (n)
      RETURN n
      """
    Then the result should be, in any order:
      | n                         |
      | (:Person {name: 'Ada'})   |

  Scenario: [2] Insert a path with an edge pointing right
    Given an empty graph
    When executing query:
      """
      INSERT (:A)-[:T {w: 1}]->(:B)
      """
    Then the result should be empty
    And the side effects should be:
      | +nodes         | 2 |
      | +relationships | 1 |
      | +labels        | 2 |
      | +properties    | 1 |
    When executing control query:
      """
      MATCH (a)-[r]->(b)
      RETURN a, r, b
      """
    Then the result should be, in any order:
      | a    | r             | b    |
      | (:A) | [:T {w: 1}]   | (:B) |

  Scenario: [3] Insert a path with an edge pointing left
    Given an empty graph
    When executing query:
      """
      INSERT (:A)<-[:T]-(:B)
      """
    Then the result should be empty
    And the side effects should be:
      | +nodes         | 2 |
      | +relationships | 1 |
      | +labels        | 2 |
    When executing control query:
      """
      MATCH (a)-[:T]->(b)
      RETURN a, b
      """
    Then the result should be, in any order:
      | a    | b    |
      | (:B) | (:A) |

  Scenario: [4] Insert a list of path patterns that share a variable
    Given an empty graph
    When executing query:
      """
      INSERT (a:A), (a)-[:T]->(:B), (a)-[:T]->(:C)
      """
    Then the result should be empty
    And the side effects should be:
      | +nodes         | 3 |
      | +relationships | 2 |
      | +labels        | 3 |
    When executing control query:
      """
      MATCH (:A)-[:T]->(x)
      RETURN labels(x) AS l
      """
    Then the result should be, in any order:
      | l     |
      | ['B'] |
      | ['C'] |

  Scenario: [5] Insert an edge between matched nodes
    Given an empty graph
    And having executed:
      """
      INSERT (:A {k: 1}), (:B {k: 2})
      """
    When executing query:
      """
      MATCH (a:A), (b:B)
      INSERT (a)-[:T]->(b)
      """
    Then the result should be empty
    And the side effects should be:
      | +relationships | 1 |
    When executing control query:
      """
      MATCH (a)-[:T]->(b)
      RETURN a.k, b.k
      """
    Then the result should be, in any order:
      | a.k | b.k |
      | 1   | 2   |

  Scenario: [6] Insert once per incoming row
    Given an empty graph
    When executing query:
      """
      UNWIND [1, 2, 3] AS i
      INSERT (:N {i: i})
      """
    Then the result should be empty
    And the side effects should be:
      | +nodes      | 3 |
      | +labels     | 1 |
      | +properties | 3 |

  Scenario: [7] Return what an insert bound
    Given an empty graph
    When executing query:
      """
      INSERT (n:N {k: 1})
      RETURN n.k AS k
      """
    Then the result should be, in any order:
      | k |
      | 1 |
    And the side effects should be:
      | +nodes      | 1 |
      | +labels     | 1 |
      | +properties | 1 |

  Scenario: [8] Insert keywords are case-insensitive
    Given an empty graph
    When executing query:
      """
      insert (:N)
      """
    Then the result should be empty
    And the side effects should be:
      | +nodes  | 1 |
      | +labels | 1 |
