// Simulated assistant. Swap `answerFor` for a real model call when the backend exists;
// the UI only depends on the `steps` + streamed `answer` shape.

export interface Reply {
  steps: string[];
  answer: string;
}

const replies: { match: RegExp; reply: Reply }[] = [
  {
    match: /slow|latency|fast|cold/i,
    reply: {
      steps: ["Reading p95 traces across 32 regions", "Comparing cold starts by function", "Simulating edge placement"],
      answer:
        "Your `checkout` function cold-starts in 412ms from iad1 only. Pinning it to 6 edge regions near your traffic cuts p95 from 640ms to 118ms. Want me to roll that out behind a 10% canary?",
    },
  },
  {
    match: /cost|price|bill|spend/i,
    reply: {
      steps: ["Aggregating last 30 days of invocations", "Finding idle provisioned memory", "Projecting next month"],
      answer:
        "You're on track for $184 this month. Three functions are over-provisioned at 1024MB but peak at 190MB — right-sizing them saves about $41/mo with no latency change.",
    },
  },
  {
    match: /deploy|ship|release|rollback/i,
    reply: {
      steps: ["Diffing main against production", "Running 128 checks", "Planning a zero-downtime rollout"],
      answer:
        "main is 4 commits ahead of production. All checks pass. I'll roll it out region by region — iad1 first, then the rest over 90 seconds — and auto-rollback if error rate rises above 0.5%.",
    },
  },
];

const fallback: Reply = {
  steps: ["Understanding the question", "Searching your project", "Drafting a plan"],
  answer:
    "roll runs your code on 32 edge regions with zero servers to manage. Ask me about latency, cost, or deploys — I can inspect traces, right-size functions, and ship changes safely.",
};

export function answerFor(prompt: string): Reply {
  return replies.find((r) => r.match.test(prompt))?.reply ?? fallback;
}

export const suggestions = ["Why is checkout slow?", "How can I cut my bill?", "Deploy main to production"];
