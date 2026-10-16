-- Fixture for the Tabularis SurrealDB plugin.
--
-- Covers a SCHEMAFULL and a SCHEMALESS table, nested objects and arrays,
-- record ids of several shapes, a relation (graph edge) table, numeric edge
-- cases, NONE vs NULL, a table name that needs quoting, enough rows to
-- paginate, and a read-only database user for permission errors.
--
-- Idempotent: it removes what it defines first. Run it inside the target
-- namespace/database, e.g.
--
--   surreal sql --endpoint ws://127.0.0.1:8000 --user root --pass root \
--     --ns test --db test --hide-welcome < fixture/seed.surql
--
-- (`surreal import` is not suitable: on 3.x it requires OPTION IMPORT, which
-- skips field processing such as DEFAULT values.) The live test
-- (tests/live_db.rs) loads this file through the plugin itself.
--
-- Avoids syntax that changed between SurrealDB 2.x and 3.x. Verified on 3.2.3.

REMOVE TABLE IF EXISTS person;
REMOVE TABLE IF EXISTS event;
REMOVE TABLE IF EXISTS knows;
REMOVE TABLE IF EXISTS item;
REMOVE TABLE IF EXISTS marker;
REMOVE TABLE IF EXISTS `odd table`;
REMOVE USER IF EXISTS reader ON DATABASE;

-- SCHEMAFULL: columns come from the declared fields.
DEFINE TABLE person SCHEMAFULL COMMENT 'People (schemafull)';
DEFINE FIELD name ON person TYPE string COMMENT 'Display name';
DEFINE FIELD age ON person TYPE option<int>;
DEFINE FIELD address ON person TYPE object;
DEFINE FIELD address.city ON person TYPE string;
DEFINE FIELD address.zip ON person TYPE option<string>;
DEFINE FIELD tags ON person TYPE array<string> DEFAULT [];
DEFINE FIELD balance ON person TYPE decimal DEFAULT 0dec;
DEFINE INDEX person_name ON person FIELDS name UNIQUE;
DEFINE INDEX person_city_age ON person FIELDS address.city, age;

CREATE person:alice CONTENT {
    name: 'Alice', age: 30, address: { city: 'Rome', zip: '00184' },
    tags: ['admin', 'ops'], balance: 12.345678901234567890123dec
};
CREATE person:bob CONTENT { name: 'Bob', address: { city: 'Oslo' }, balance: 1dec };
CREATE person:carol CONTENT { name: 'Carol', age: 41, address: { city: 'Lima' } };

-- Graph edges between people.
DEFINE TABLE knows TYPE RELATION IN person OUT person SCHEMALESS;
RELATE person:alice->knows->person:bob SET since = 2020;
RELATE person:bob->knows->person:carol SET since = 2021;

-- SCHEMALESS with heterogeneous records: columns are sampled from data.
DEFINE TABLE event SCHEMALESS COMMENT 'Heterogeneous events';
DEFINE FIELD kind ON event TYPE string;
CREATE event:1 CONTENT {
    kind: 'signup',
    actor: person:alice,
    payload: { steps: [1, 2, { ok: true, note: NULL }], meta: {} },
    big: 9223372036854775807,
    ratio: 1.5f,
    exact: 0.1dec,
    nothing: NULL,
    at: d'2024-01-02T03:04:05.123456789Z',
    uid: u'0190d6c0-0000-7000-8000-000000000001',
    took: 1h30m
};
CREATE event:⟨weird-id⟩ CONTENT { kind: 'odd', extra: 'only here' };
CREATE event:[2024, 'q1'] CONTENT { kind: 'array-id' };
CREATE event:{ region: 'eu', seq: 7 } CONTENT { kind: 'object-id' };

-- A table whose name needs quoting.
DEFINE TABLE `odd table` SCHEMALESS;
CREATE `odd table`:x CONTENT { `odd field`: 'y' };

-- Rows for pagination tests: item:1 .. item:25.
DEFINE TABLE item SCHEMALESS;
FOR $i IN [1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16, 17, 18, 19, 20, 21, 22, 23, 24, 25] {
    CREATE item CONTENT { id: $i, n: $i };
};

-- Lets two connections to different databases tell which one they read.
DEFINE TABLE marker SCHEMALESS;

-- A read-only database user, for permission errors.
DEFINE USER reader ON DATABASE PASSWORD 'reader-pass' ROLES VIEWER;
