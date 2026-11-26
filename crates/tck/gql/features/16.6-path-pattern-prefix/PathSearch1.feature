#encoding: utf-8

Feature: PathSearch1 - ISO/IEC 39075:2024 16.6 <path pattern prefix>, <path search prefix>

  Background:
    Given an empty graph
    And having executed:
      """
      INSERT (a:N {k: 1})-[:R]->(b:N {k: 2})-[:R]->(c:N {k: 3})-[:R]->(d:N {k: 4}),
             (a)-[:R]->(e:N {k: 5})-[:R]->(d),
             (a)-[:R]->(f:N {k: 6})-[:R]->(d),
             (d)-[:R]->(a)
      """

  Scenario: [1] ANY SHORTEST keeps one shortest path
    When executing query:
      """
      MATCH p = ANY SHORTEST (x:N {k: 1})-[:R]->+(y:N {k: 4})
      RETURN length(p) AS length, size([n IN nodes(p) WHERE n.k IN [5, 6]]) AS middle
      """
    Then the result should be, in any order:
      | length | middle |
      | 2      | 1      |
    And no side effects

  Scenario: [2] ALL SHORTEST keeps every path of the shortest length
    When executing query:
      """
      MATCH p = ALL SHORTEST (x:N {k: 1})-[:R]->+(y:N {k: 4})
      RETURN [n IN nodes(p) | n.k] AS ks
      """
    Then the result should be, in any order:
      | ks        |
      | [1, 5, 4] |
      | [1, 6, 4] |
    And no side effects

  Scenario: [3] SHORTEST k keeps that many paths, shortest first
    When executing query:
      """
      MATCH p = SHORTEST 3 (x:N {k: 1})-[:R]->+(y:N {k: 4})
      RETURN length(p) AS length, count(*) AS paths
      """
    Then the result should be, in any order:
      | length | paths |
      | 2      | 2     |
      | 3      | 1     |
    And no side effects

  Scenario: [4] SHORTEST k GROUPS keeps every path of that many shortest lengths
    When executing query:
      """
      MATCH p = SHORTEST 2 GROUPS (x:N {k: 1})-[:R]->+(y:N {k: 4})
      RETURN length(p) AS length, count(*) AS paths
      """
    Then the result should be, in any order:
      | length | paths |
      | 2      | 2     |
      | 3      | 1     |
    And no side effects

  Scenario: [5] ANY k keeps at most that many paths
    When executing query:
      """
      MATCH p = ANY 2 (x:N {k: 1})-[:R]->+(y:N {k: 4})
      RETURN count(*) AS paths
      """
    Then the result should be, in any order:
      | paths |
      | 2     |
    And no side effects

  Scenario: [6] A selector chooses among the paths of each pair of end nodes
    When executing query:
      """
      MATCH p = ANY SHORTEST (x:N {k: 1})-[:R]->+(y:N)
      RETURN y.k AS k, length(p) AS length
      ORDER BY k
      """
    Then the result should be, in order:
      | k | length |
      | 1 | 3      |
      | 2 | 1      |
      | 3 | 2      |
      | 4 | 2      |
      | 5 | 1      |
      | 6 | 1      |
    And no side effects

  Scenario: [7] A selector chooses before the WHERE of its MATCH filters
    When executing query:
      """
      MATCH p = ANY SHORTEST (x:N {k: 1})-[:R]->+(y:N {k: 4})
      WHERE length(p) > 2
      RETURN length(p) AS length
      """
    Then the result should be, in any order:
      | length |
    And no side effects

  Scenario: [8] A path of zero hops is the shortest from a node to itself under an asterisk
    When executing query:
      """
      MATCH p = ANY SHORTEST (x:N {k: 1})-[:R]->*(x)
      RETURN length(p) AS length
      """
    Then the result should be, in any order:
      | length |
      | 0      |
    And no side effects

  Scenario: [9] A selector finds the shortest closed path back to the start
    When executing query:
      """
      MATCH p = ALL SHORTEST (x:N {k: 1})-[:R]->+(x)
      RETURN [n IN nodes(p) | n.k] AS ks
      """
    Then the result should be, in any order:
      | ks           |
      | [1, 5, 4, 1] |
      | [1, 6, 4, 1] |
    And no side effects

  Scenario: [10] An edge pattern pointing left searches against the edge direction
    When executing query:
      """
      MATCH p = ANY SHORTEST (x:N {k: 2})<-[:R]-+(y:N {k: 6})
      RETURN [n IN nodes(p) | n.k] AS ks
      """
    Then the result should be, in any order:
      | ks           |
      | [2, 1, 4, 6] |
    And no side effects

  Scenario: [11] Selector keywords are case-insensitive and take PATH or PATHS
    When executing query:
      """
      MATCH p = any shortest path (x:N {k: 1})-[:R]->+(y:N {k: 4})
      RETURN length(p) AS length
      """
    Then the result should be, in any order:
      | length |
      | 2      |
    And no side effects

  Scenario: [12] path_length gives the number of edges of a path
    When executing query:
      """
      MATCH p = ANY SHORTEST (x:N {k: 2})-[:R]->+(y:N {k: 6})
      RETURN path_length(p) AS length
      """
    Then the result should be, in any order:
      | length |
      | 4      |
    And no side effects
