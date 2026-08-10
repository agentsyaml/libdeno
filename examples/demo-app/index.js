// Demo entry: mixes an npm package, a node builtin, a local module and a JSON import.
import chalk from "chalk";
import { join } from "node:path";
import { add } from "./math.js";
import pkg from "./pkg.json" with { type: "json" };

console.log(chalk.green("npm package (chalk) works"));
console.log(chalk.cyan(`node builtin (node:path): ${join("a", "b", "c")}`));
console.log(chalk.yellow(`local module: 1 + 2 = ${add(1, 2)}`));
console.log(chalk.magenta(`json import: name=${pkg.name} deps=${pkg.dependencies.length}`));
