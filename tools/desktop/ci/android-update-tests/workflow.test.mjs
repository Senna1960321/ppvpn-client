// Run the actual release predicates with GitHub's expression implementation.
import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import test from "node:test";
import { data, Evaluator, Lexer, Parser } from "@actions/expressions";
import { truthy } from "@actions/expressions/result";
import { parse } from "yaml";

const { jobs } = parse(readFileSync(new URL("../../../../.github/workflows/release.yml", import.meta.url), "utf8"));

function values() {
  const needs = Object.fromEntries(jobs["release-files"].needs.map(name => [name, { result: "success" }]));
  needs.prepare.outputs = { channel: "stable", publish: "true" };
  return { needs, inputs: { channel: "stable" }, vars: { ANDROID_APK_SIGNER_SHA256: "ab".repeat(32) } };
}

function enabled(job, context, cancelled = false) {
  const functions = [{ name: "cancelled", minArgs: 0, maxArgs: 0, call: () => new data.BooleanData(cancelled) }];
  const expression = new Parser(new Lexer(jobs[job].if).lex().tokens, Object.keys(context), functions).parse();
  return truthy(new Evaluator(expression, JSON.parse(JSON.stringify(context), data.reviver),
    new Map(functions.map(fn => [fn.name, fn]))).evaluate());
}

test("APK signing requires stable approval and a pin; release publication tolerates only optional APK absence", () => {
  assert.equal(enabled("android-apk", values()), true);
  assert.equal(jobs["android-apk"].environment, "desktop-release-stable");
  for (const approval of ["", "skipped", "failure", "cancelled"]) {
    const context = values();
    context.needs.approve.result = approval;
    assert.equal(enabled("android-apk", context), false, `approval ${approval}`);
    assert.equal(enabled("release-files", context), false, `publication approval ${approval}`);
  }
  for (const channel of ["dev", "site", ""]) {
    const context = values();
    context.needs.prepare.outputs.channel = channel;
    context.inputs.channel = channel;
    assert.equal(enabled("android-apk", context), false, channel);
    assert.equal(enabled("release-files", context), false, channel);
  }
  const noPin = values();
  noPin.vars = {};
  assert.equal(enabled("android-apk", noPin), false);
  noPin.needs["android-apk"].result = "skipped";
  assert.equal(enabled("release-files", noPin), true, "Historical releases without APK remain publishable");
  for (const result of ["success", "skipped", "failure", "cancelled", ""]) {
    const context = values();
    context.needs["android-apk"].result = result;
    assert.equal(enabled("release-files", context), ["success", "skipped"].includes(result), result);
  }
  for (const dependency of jobs["release-files"].needs.filter(name => name !== "android-apk")) {
    for (const result of ["failure", "skipped", "cancelled"]) {
      const context = values();
      context.needs[dependency].result = result;
      assert.equal(enabled("release-files", context), false, `${dependency} ${result}`);
    }
  }
  const unpublished = values();
  unpublished.needs.prepare.outputs.publish = "false";
  assert.equal(enabled("release-files", unpublished), false);
  for (const job of ["android-apk", "release-files"]) assert.equal(enabled(job, values(), true), false, job);
});
