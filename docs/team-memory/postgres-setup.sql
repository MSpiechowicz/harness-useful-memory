-- Creates the database and the least-privilege role Harness Memory needs on a PostgreSQL server (version 15 or newer).
-- Run it once as a superuser or a role with CREATEDB and CREATEROLE, and pass the new role's password. It is safe to run
-- again: it keeps what exists and only resets the role's password.
--
--   psql "postgres://postgres@db.example.com/postgres?sslmode=require" \
--        -v memory_password='a-long-random-password' -f docs/team-memory/postgres-setup.sql
--
-- The role may log in, own the one database and create tables in it (the server creates its own tables on first use). It
-- is no superuser, cannot create databases or roles, and nobody else can connect to the database. To keep several teams
-- apart, run this once per team with another database and role name (replace harness_memory below).
-- Then give each teammate this URL:
--   postgres://harness_memory:<password>@db.example.com:5432/harness_memory?sslmode=require

\set ON_ERROR_STOP on

-- The role, created if missing, with the password set every time.
SELECT 'CREATE ROLE harness_memory LOGIN NOSUPERUSER NOCREATEDB NOCREATEROLE NOREPLICATION'
WHERE NOT EXISTS (SELECT FROM pg_roles WHERE rolname = 'harness_memory')
\gexec

SELECT format('ALTER ROLE harness_memory PASSWORD %L', :'memory_password')
\gexec

-- The database, created if missing.
SELECT 'CREATE DATABASE harness_memory OWNER harness_memory'
WHERE NOT EXISTS (SELECT FROM pg_database WHERE datname = 'harness_memory')
\gexec

-- Only the role (and superusers) may connect.
REVOKE ALL ON DATABASE harness_memory FROM PUBLIC;
GRANT CONNECT ON DATABASE harness_memory TO harness_memory;

-- Inside the database, nobody but the role may use or create in the public schema.
\connect harness_memory
REVOKE ALL ON SCHEMA public FROM PUBLIC;
GRANT ALL ON SCHEMA public TO harness_memory;
