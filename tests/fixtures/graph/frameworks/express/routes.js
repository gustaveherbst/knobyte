const express = require('express');
const router = express.Router();
const app = express();

function listUsers(req, res) {
  res.json([]);
}

function requireAuth(req, res, next) {
  next();
}

async function createUser(req, res) {
  res.status(201).end();
}

app.get('/users', listUsers);
router.post('/users', requireAuth, createUser);
app.delete("/users/:id", (req, res) => res.end());

module.exports = { app, router };
