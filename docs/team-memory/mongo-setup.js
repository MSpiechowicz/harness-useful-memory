// Creates the least-privilege user Harness Memory needs on a MongoDB deployment: readWrite on one database, nothing else.
// Run it once as an admin user (the server creates its own collections and indexes on first use). It is safe to run
// again: it changes the password of an existing user.
//
//   HARNESS_MEMORY_DB_PASSWORD='a-long-random-password' \
//     mongosh "mongodb://admin@db.example.com/admin?tls=true" --file docs/team-memory/mongo-setup.js
//
// To keep several teams apart, run it once per team with another database and user name (replace harness_memory below).
// Then give each teammate this URL (the database in the path is the one the notes live in):
//   mongodb://harness_memory:<password>@db.example.com:27017/harness_memory?tls=true&replicaSet=rs0

const database = "harness_memory";
const user = "harness_memory";
const password = process.env.HARNESS_MEMORY_DB_PASSWORD;

if (!password) {
  throw new Error("Set HARNESS_MEMORY_DB_PASSWORD to the password for the new user.");
}

const target = db.getSiblingDB(database);

if (target.getUser(user)) {
  target.updateUser(user, { pwd: password, roles: [{ role: "readWrite", db: database }] });
} else {
  target.createUser({ user, pwd: password, roles: [{ role: "readWrite", db: database }] });
}
