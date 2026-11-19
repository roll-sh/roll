import { describe, expect, it } from "vitest";
import {
  optionValue,
  PROCESSOR_FORMS,
  pipelineFromJson,
  pipelineToJson,
  pipelineUsers,
  processorProblem,
  sampleDocs,
  simulateResults,
  taggedForSimulate,
} from "./search-pipelines";

describe("the processor form", () => {
  it("round-trips a pipeline's JSON, options it doesn't know included, without the dates the cluster keeps", () => {
    const json = {
      description: "web logs",
      processors: [
        { grok: { field: "message", patterns: ["%{IP:ip}"], ecs_compatibility: "v1", if: "ctx.message != null", tag: "parse", on_failure: [{ set: { field: "error", value: "{{_ingest.on_failure_message}}" } }] } },
        { geoip: { field: "ip", ignore_failure: true } },
        { script: "ctx.n = 1" },
      ],
      on_failure: [{ fail: { message: "boom" } }],
      version: 3,
      _meta: { owner: "web" },
    };
    const p = pipelineFromJson({ ...json, created_date_millis: 1, modified_date_millis: 2 });
    expect(p.processors.map((x) => x.type)).toEqual(["grok", "geoip", "script"]);
    expect(p.processors[0].onFailure?.[0].options).toEqual({ field: "error", value: "{{_ingest.on_failure_message}}" });
    expect(pipelineToJson(p)).toEqual({ ...json, processors: [json.processors[0], json.processors[1], { script: { source: "ctx.n = 1" } }] });
  });

  it("reads fields as people type them", () => {
    const spec = (type: string, key: string) => PROCESSOR_FORMS[type].find((s) => s.key === key)!;
    expect(optionValue(spec("set", "value"), "42")).toBe(42);
    expect(optionValue(spec("set", "value"), "hello")).toBe("hello");
    expect(optionValue(spec("remove", "field"), "a\n b \n")).toEqual(["a", "b"]);
    expect(optionValue(spec("remove", "field"), "a")).toBe("a");
    expect(processorProblem({ type: "rename", options: { field: "a" } })).toBe("[rename] needs target_field.");
  });
});

describe("simulate", () => {
  it("takes documents or bare sources", () => {
    expect(sampleDocs('[{"a":1},{"_index":"i","_source":{"b":2}}]')).toEqual({ docs: [{ _source: { a: 1 } }, { _index: "i", _source: { b: 2 } }] });
    expect(sampleDocs("[1]")).toEqual({ error: "Each document is a JSON object." });
  });

  it("maps each verbose result to its processor and says what it changed", () => {
    const pipeline = pipelineFromJson({ processors: [{ set: { field: "a", value: "x" } }, { lowercase: { field: "m", tag: "low", on_failure: [{ set: { field: "err", value: "e" } }] } }, { drop: { if: "ctx.b == 2" } }] });
    const { body, paths } = taggedForSimulate(pipeline);
    expect((body.processors as Record<string, { tag: string }>[]).map((p) => Object.values(p)[0].tag)).toEqual(["__roll_0", "low", "__roll_2"]);
    const docs = [{ _source: { b: 1 } }, { _source: { b: 2 } }];
    const res = simulateResults(
      docs,
      {
        docs: [
          {
            processor_results: [
              { processor_type: "set", status: "success", tag: "__roll_0", doc: { _source: { a: "x", b: 1 } } },
              { processor_type: "lowercase", status: "error", tag: "low", error: { reason: "field [m] not present" } },
              { processor_type: "set", status: "success", tag: "__roll_1.on_failure.0", doc: { _source: { a: "x", b: 1, err: "e" } } },
              { processor_type: "drop", status: "skipped", tag: "__roll_2" },
            ],
          },
          { processor_results: [{ processor_type: "set", status: "success", tag: "__roll_0", doc: { _source: { a: "x", b: 2 } } }, { processor_type: "drop", status: "dropped", tag: "__roll_2" }] },
        ],
      },
      paths,
    );
    expect(res[0].steps.map((s) => [s.path, s.status, s.changed])).toEqual([
      ["0", "success", ["a"]],
      ["1", "error", []],
      ["1.on_failure.0", "success", ["err"]],
      ["2", "skipped", []],
    ]);
    expect(res[0].output).toEqual({ a: "x", b: 1, err: "e" });
    expect(res[1].output).toBeNull();
  });
});

describe("indices' pipelines", () => {
  it("lists the indices that run each pipeline", () => {
    expect(pipelineUsers({ b: { settings: { index: { default_pipeline: "web" } } }, a: { settings: { index: { default_pipeline: "web", final_pipeline: "audit" } } }, c: { settings: { index: { default_pipeline: "_none" } } } })).toEqual({
      web: [
        { index: "a", as: "default" },
        { index: "b", as: "default" },
      ],
      audit: [{ index: "a", as: "final" }],
    });
  });
});
