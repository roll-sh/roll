import { connection } from "next/server";

/** The current time in Unix seconds, read at request time (Cache Components needs the explicit `connection()`). */
export async function requestNow() {
  await connection();
  return Date.now() / 1000;
}

