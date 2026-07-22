CREATE TABLE IF NOT EXISTS realms (
    id SERIAL PRIMARY KEY,
    name TEXT NOT NULL UNIQUE
);

ALTER TABLE nodes
ADD COLUMN realm_id INTEGER REFERENCES realms(id);

ALTER TABLE edges
ADD COLUMN realm_id INTEGER REFERENCES realms(id);

ALTER TABLE edges
ADD CONSTRAINT edge_type_operates_on CHECK (edge_type = 'operates_on');

ALTER TABLE realms
ADD CONSTRAINT realm_id_pk CHECK (realm_id = 1);
