// Evaluate the production workflow with GitHub's own expression library.
import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import test from "node:test";
import { data, Evaluator, Lexer, Parser } from "@actions/expressions";
import { truthy } from "@actions/expressions/result";
import { parse } from "yaml";

const { jobs } = parse(readFileSync(new URL("../../../.github/workflows/release.yml", import.meta.url), "utf8"));
const compileJobs = ["linux", "windows-app", "windows-service", "macos-app", "macos-service", "cli-linux", "cli-macos-build"];
const packageJobs = ["windows", "macos", "cli-macos"];
const azureVariables = ["AZURE_CLIENT_ID", "AZURE_TENANT_ID", "AZURE_SUBSCRIPTION_ID", "ARTIFACT_SIGNING_ENDPOINT", "ARTIFACT_SIGNING_ACCOUNT", "ARTIFACT_SIGNING_PROFILE"];

function context(channel, overrides = {}) {
  const prepare = {
    result: "success",
    outputs: {
      channel: channel === "site" ? "" : channel,
      package: channel === "site" ? "false" : "true",
      environment: ["dev", "stable"].includes(channel) ? `desktop-release-${channel}` : "",
      publish: channel === "stable" ? "true" : "false",
    },
  };
  return {
    needs: {
      prepare,
      approve: { result: channel === "stable" || channel === "site" ? "success" : "skipped" },
      ...Object.fromEntries(compileJobs.map(name => [name, { result: "success" }])),
    },
    github: { event_name: channel ? "workflow_dispatch" : "push" },
    inputs: { channel },
    vars: {},
    secrets: {},
    ...overrides,
  };
}

function evaluate(expression, values, cancelled = false) {
  assert.equal(typeof expression, "string");
  const source = expression.trim().replace(/^\$\{\{\s*|\s*\}\}$/g, "");
  const functions = [{ name: "cancelled", minArgs: 0, maxArgs: 0, call: () => new data.BooleanData(cancelled) }];
  const tree = new Parser(new Lexer(source).lex().tokens, Object.keys(values), functions).parse();
  const inputs = JSON.parse(JSON.stringify(values), data.reviver);
  return new Evaluator(tree, inputs, new Map(functions.map(fn => [fn.name, fn]))).evaluate();
}

function condition(job, values, cancelled = false) {
  return truthy(evaluate(jobs[job].if, values, cancelled));
}

function variable(job, key, values) {
  return evaluate(jobs[job].env[key], values).coerceString();
}

test("stable compilation can start while approval is pending, with only prepare as a dependency", () => {
  for (const job of compileJobs) {
    assert.deepEqual([jobs[job].needs].flat(), ["prepare"], job);
    for (const approval of ["", "success", "failure", "cancelled", "skipped"]) {
      const values = context("stable");
      values.needs.approve.result = approval;
      assert.equal(condition(job, values), true, `${job}: ${approval}`);
    }
  }
});

test("compilation refuses site-only runs, failed preparation and cancellation", () => {
  for (const job of compileJobs) {
    assert.equal(condition(job, context("site")), false, job);
    for (const result of ["failure", "cancelled", "skipped"]) {
      const values = context("stable");
      values.needs.prepare.result = result;
      assert.equal(condition(job, values), false, `${job}: ${result}`);
    }
    assert.equal(condition(job, context("stable"), true), false, job);
  }
});

test("signing requires approval and every compile dependency; dev and merge may skip approval", () => {
  for (const job of packageJobs) {
    const dependencies = [jobs[job].needs].flat();
    assert.ok(dependencies.includes("approve"), job);
    for (const channel of ["stable", "dev", ""]) {
      assert.equal(condition(job, context(channel)), true, `${job}: ${channel}`);
    }
    for (const approval of ["", "failure", "cancelled"]) {
      const values = context("stable");
      values.needs.approve.result = approval;
      assert.equal(condition(job, values), false, `${job}: ${approval}`);
    }
    for (const dependency of dependencies.filter(name => compileJobs.includes(name))) {
      for (const result of ["failure", "cancelled", "skipped"]) {
        const values = context("stable");
        values.needs[dependency].result = result;
        assert.equal(condition(job, values), false, `${job}: ${dependency} ${result}`);
      }
    }
    assert.equal(condition(job, context("stable"), true), false, job);
  }
  // A successful stable prepare cannot skip approve's own condition.
  assert.equal(condition("approve", context("stable")), true);
  assert.equal(condition("approve", context("site")), true);
  assert.equal(condition("approve", context("dev")), false);
  assert.equal(condition("approve", context("")), false);
});

test("only dev compilation enters an environment and reads its private backend", () => {
  for (const job of compileJobs) {
    for (const channel of ["stable", "dev", ""]) {
      const values = context(channel, {
        vars: { PPVPN_API_BASE: "https://production.example.test" },
        secrets: { PPVPN_API_BASE: "https://private-dev.example.test" },
      });
      const environment = jobs[job].environment === undefined ? "" : evaluate(jobs[job].environment, values).coerceString();
      assert.equal(environment, channel === "dev" && job !== "macos-service" ? "desktop-release-dev" : "", job);
      if (jobs[job].env?.PPVPN_API_BASE_DEFAULT) {
        assert.equal(variable(job, "PPVPN_API_BASE_DEFAULT", values),
          channel === "dev" ? values.secrets.PPVPN_API_BASE : values.vars.PPVPN_API_BASE, job);
      }
    }
  }
});

test("stable Windows services enforce signatures using public pins without Azure environment variables", () => {
  for (const pin of ["", "1.3.6.1.4.1.311.97.100"]) {
    const values = context("stable", { vars: {
      PPVPN_WINDOWS_PUBLISHER_EKU: pin,
      PPVPN_WINDOWS_NEXT_PUBLISHER_EKU: "1.3.6.1.4.1.311.97.200",
    } });
    assert.equal(variable("windows-service", "PPVPN_WINDOWS_ALLOW_UNSIGNED_CLIENT", values), "0");
    assert.equal(variable("windows-service", "PPVPN_WINDOWS_PUBLISHER_EKU", values), pin);
    assert.equal(variable("windows-service", "PPVPN_WINDOWS_NEXT_PUBLISHER_EKU", values), values.vars.PPVPN_WINDOWS_NEXT_PUBLISHER_EKU);
  }
});

test("dev Windows signing still requires all Azure configuration and a publisher pin", () => {
  const vars = Object.fromEntries(azureVariables.map(name => [name, "configured"]));
  vars.PPVPN_WINDOWS_PUBLISHER_EKU = "1.3.6.1.4.1.311.97.100";
  assert.equal(variable("windows-service", "PPVPN_WINDOWS_ALLOW_UNSIGNED_CLIENT", context("dev", { vars })), "0");
  for (const missing of [...azureVariables, "PPVPN_WINDOWS_PUBLISHER_EKU"]) {
    const incomplete = { ...vars, [missing]: "" };
    assert.equal(variable("windows-service", "PPVPN_WINDOWS_ALLOW_UNSIGNED_CLIENT", context("dev", { vars: incomplete })), "1", missing);
  }
  assert.equal(variable("windows-service", "PPVPN_WINDOWS_ALLOW_UNSIGNED_CLIENT", context("", { vars })), "1");
});

test("Linux final artifacts retain dev and merge retention while stable intermediates expire sooner", () => {
  for (const job of ["linux", "cli-linux"]) {
    const upload = jobs[job].steps.find(step => step.uses?.startsWith("actions/upload-artifact@"));
    for (const [channel, days] of [["stable", "1"], ["dev", "3"], ["", "0"]]) {
      assert.equal(evaluate(upload.with["retention-days"], context(channel)).coerceString(), days, `${job}: ${channel}`);
    }
  }
});
