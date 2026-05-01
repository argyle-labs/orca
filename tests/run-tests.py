#!/usr/bin/env python3
"""
Brain Agent Validation Test Suite

Runs test cases against any OpenAI-compatible API to validate
model viability as a brain backend and detect regressions.

Usage:
    # LM Studio (default)
    python3 run-tests.py

    # Ollama
    python3 run-tests.py --backend ollama

    # Custom endpoint
    python3 run-tests.py --base-url http://localhost:8080/v1 --model my-model

    # Run specific category
    python3 run-tests.py --category agent-routing

    # Run specific test
    python3 run-tests.py --test routing-001

    # Verbose output (show model responses)
    python3 run-tests.py -v
"""

import argparse
import json
import os
import sys
import time
from datetime import datetime
from pathlib import Path
from urllib.request import Request, urlopen
from urllib.error import URLError

SCRIPT_DIR = Path(__file__).parent
FIXTURES_DIR = SCRIPT_DIR / "fixtures"
RESULTS_DIR = SCRIPT_DIR / "results"

BACKENDS = {
    "lmstudio": {"base_url": "http://localhost:1234/v1", "default_model": None},
    "ollama": {"base_url": "http://localhost:11434/v1", "default_model": None},
}


def api_request(base_url: str, path: str, payload: dict, timeout: int = 120) -> dict:
    """Make a request to the OpenAI-compatible API."""
    url = f"{base_url}{path}"
    data = json.dumps(payload).encode()
    req = Request(url, data=data, headers={"Content-Type": "application/json"})
    try:
        with urlopen(req, timeout=timeout) as resp:
            return json.loads(resp.read())
    except URLError as e:
        raise ConnectionError(f"Cannot reach {url}: {e}")


def discover_model(base_url: str) -> str:
    """Get the first available model from the API."""
    try:
        url = f"{base_url}/models"
        req = Request(url)
        with urlopen(req, timeout=10) as resp:
            data = json.loads(resp.read())
            models = data.get("data", [])
            if not models:
                raise RuntimeError("No models loaded")
            model_id = models[0]["id"]
            return model_id
    except URLError as e:
        raise ConnectionError(f"Cannot list models at {base_url}/models: {e}")


def chat(base_url: str, model: str, system: str | None, user_prompt: str, temperature: float = 0.1) -> str:
    """Send a chat completion request and return the assistant response."""
    messages = []
    if system:
        messages.append({"role": "system", "content": system})
    messages.append({"role": "user", "content": user_prompt})

    payload = {
        "model": model,
        "messages": messages,
        "temperature": temperature,
        "max_tokens": 1024,
    }
    result = api_request(base_url, "/chat/completions", payload)
    return result["choices"][0]["message"]["content"].strip()


def load_fixture(filename: str) -> str:
    """Load a fixture file."""
    path = FIXTURES_DIR / filename
    if not path.exists():
        raise FileNotFoundError(f"Fixture not found: {path}")
    return path.read_text()


def validate_contains(response: str, expected: str) -> tuple[bool, str]:
    text = extract_content(response)
    if expected.lower() in text.lower():
        return True, f"Contains '{expected}'"
    return False, f"Missing '{expected}'"


def validate_not_contains(response: str, expected: str) -> tuple[bool, str]:
    text = extract_content(response)
    if expected.lower() not in text.lower():
        return True, f"Does not contain '{expected}'"
    return False, f"Unexpectedly contains '{expected}'"


def validate_one_of(response: str, expected: list[str]) -> tuple[bool, str]:
    lower = extract_content(response).lower()
    for item in expected:
        if item.lower() in lower:
            return True, f"Contains '{item}'"
    return False, f"None of {expected} found"


def extract_content(response: str) -> str:
    """Strip reasoning model artifacts (<think> blocks, markdown fences)."""
    import re
    text = response.strip()
    # Strip <think>...</think> blocks (reasoning models)
    text = re.sub(r'<think>.*?</think>', '', text, flags=re.DOTALL).strip()
    # Strip markdown fences
    if text.startswith("```"):
        lines = text.split("\n")
        lines = [l for l in lines if not l.startswith("```")]
        text = "\n".join(lines).strip()
    return text


def validate_json_valid(response: str) -> tuple[bool, str]:
    text = extract_content(response)
    try:
        json.loads(text)
        return True, "Valid JSON"
    except json.JSONDecodeError as e:
        return False, f"Invalid JSON: {e}"


def validate_json_has_keys(response: str, expected: list[str]) -> tuple[bool, str]:
    text = extract_content(response)
    try:
        data = json.loads(text)
        if isinstance(data, list):
            data = data[0] if data else {}
        missing = [k for k in expected if k not in data]
        if missing:
            return False, f"Missing keys: {missing}"
        return True, f"Has all keys: {expected}"
    except json.JSONDecodeError:
        return False, "Cannot check keys — invalid JSON"


def validate_llm_judge(response: str, criteria: str, base_url: str, model: str) -> tuple[bool, str]:
    """Use the model itself to judge whether its response meets criteria."""
    judge_prompt = (
        f"You are a test validator. A model was asked a question and gave this response:\n\n"
        f"---RESPONSE---\n{response}\n---END RESPONSE---\n\n"
        f"Criteria: {criteria}\n\n"
        f"Does the response meet the criteria? Reply with ONLY 'PASS' or 'FAIL' followed by a one-sentence reason."
    )
    try:
        verdict = chat(base_url, model, None, judge_prompt, temperature=0.0)
        verdict_clean = extract_content(verdict).strip()
        passed = verdict_clean.upper().startswith("PASS")
        reason = verdict_clean.split("\n")[0][:200]
        return passed, f"Judge: {reason}"
    except Exception as e:
        return False, f"Judge error: {e}"


def run_validation(validation: dict, response: str, base_url: str = "", model: str = "") -> tuple[bool, str]:
    """Run a single validation check."""
    vtype = validation["type"]
    expected = validation.get("expected")

    if vtype == "contains":
        return validate_contains(response, expected)
    elif vtype == "not_contains":
        return validate_not_contains(response, expected)
    elif vtype == "one_of":
        return validate_one_of(response, expected)
    elif vtype == "json_valid":
        return validate_json_valid(response)
    elif vtype == "json_has_keys":
        return validate_json_has_keys(response, expected)
    elif vtype == "llm_judge":
        return validate_llm_judge(response, expected, base_url, model)
    else:
        return False, f"Unknown validation type: {vtype}"


def run_test(test: dict, base_url: str, model: str, verbose: bool) -> dict:
    """Run a single test case and return results."""
    test_id = test["id"]
    name = test["name"]

    # Build system context
    system = None
    if "system_context_file" in test:
        system = load_fixture(test["system_context_file"])

    prompt = test["prompt"].strip()
    start = time.time()

    try:
        response = chat(base_url, model, system, prompt)
    except Exception as e:
        return {
            "id": test_id,
            "name": name,
            "category": test["category"],
            "passed": False,
            "error": str(e),
            "duration_ms": int((time.time() - start) * 1000),
            "checks": [],
        }

    duration_ms = int((time.time() - start) * 1000)

    # Run all validations
    checks = []
    all_passed = True
    for v in test["validation"]:
        passed, detail = run_validation(v, response, base_url, model)
        checks.append({"type": v["type"], "passed": passed, "detail": detail})
        if not passed:
            all_passed = False

    result = {
        "id": test_id,
        "name": name,
        "category": test["category"],
        "passed": all_passed,
        "duration_ms": duration_ms,
        "checks": checks,
    }

    if verbose or not all_passed:
        result["response"] = response[:500]

    return result


def print_result(result: dict, verbose: bool):
    """Print a test result with color."""
    status = "\033[32mPASS\033[0m" if result["passed"] else "\033[31mFAIL\033[0m"
    duration = f"{result['duration_ms']}ms"
    print(f"  [{status}] {result['id']}: {result['name']} ({duration})")

    if not result["passed"] or verbose:
        for check in result["checks"]:
            mark = "\033[32m✓\033[0m" if check["passed"] else "\033[31m✗\033[0m"
            print(f"         {mark} {check['type']}: {check['detail']}")

    if "error" in result:
        print(f"         \033[31mERROR: {result['error']}\033[0m")

    if "response" in result and not result["passed"]:
        preview = result["response"][:200].replace("\n", " ")
        print(f"         Response: {preview}...")


def main():
    parser = argparse.ArgumentParser(description="Brain Agent Validation Test Suite")
    parser.add_argument("--backend", choices=["lmstudio", "ollama"], default="lmstudio")
    parser.add_argument("--base-url", help="Custom API base URL")
    parser.add_argument("--model", help="Model ID (auto-detected if omitted)")
    parser.add_argument("--category", help="Run only this category")
    parser.add_argument("--test", help="Run only this test ID")
    parser.add_argument("-v", "--verbose", action="store_true", help="Show all responses")
    parser.add_argument("--save", action="store_true", help="Save results to results/ dir")
    args = parser.parse_args()

    # Resolve backend
    if args.base_url:
        base_url = args.base_url
    else:
        base_url = BACKENDS[args.backend]["base_url"]

    # Discover model
    if args.model:
        model = args.model
    else:
        print(f"Discovering model at {base_url}...")
        try:
            model = discover_model(base_url)
        except (ConnectionError, RuntimeError) as e:
            print(f"\033[31mError: {e}\033[0m")
            print(f"\nMake sure {args.backend} is running with a model loaded.")
            sys.exit(1)

    print(f"\n{'='*60}")
    print(f"Brain Agent Validation Test Suite")
    print(f"{'='*60}")
    print(f"Backend:  {args.backend if not args.base_url else 'custom'}")
    print(f"URL:      {base_url}")
    print(f"Model:    {model}")
    print(f"Time:     {datetime.now().isoformat()}")
    print(f"{'='*60}\n")

    # Load test cases
    with open(SCRIPT_DIR / "test-cases.json") as f:
        tests = json.load(f)

    # Filter
    if args.category:
        tests = [t for t in tests if t["category"] == args.category]
    if args.test:
        tests = [t for t in tests if t["id"] == args.test]

    if not tests:
        print("No tests matched filters.")
        sys.exit(1)

    # Group by category
    categories = {}
    for t in tests:
        categories.setdefault(t["category"], []).append(t)

    # Run
    all_results = []
    total_passed = 0
    total_failed = 0

    for cat, cat_tests in categories.items():
        print(f"── {cat} ({len(cat_tests)} tests) ──")
        for test in cat_tests:
            result = run_test(test, base_url, model, args.verbose)
            all_results.append(result)
            print_result(result, args.verbose)
            if result["passed"]:
                total_passed += 1
            else:
                total_failed += 1
        print()

    # Summary
    total = total_passed + total_failed
    pass_rate = (total_passed / total * 100) if total > 0 else 0
    color = "\033[32m" if total_failed == 0 else "\033[31m"

    print(f"{'='*60}")
    print(f"Results: {color}{total_passed}/{total} passed ({pass_rate:.0f}%)\033[0m")

    # Per-category breakdown
    cat_stats = {}
    for r in all_results:
        cat = r["category"]
        cat_stats.setdefault(cat, {"passed": 0, "total": 0})
        cat_stats[cat]["total"] += 1
        if r["passed"]:
            cat_stats[cat]["passed"] += 1

    for cat, stats in cat_stats.items():
        pct = stats["passed"] / stats["total"] * 100
        c = "\033[32m" if stats["passed"] == stats["total"] else "\033[33m" if pct >= 50 else "\033[31m"
        print(f"  {cat}: {c}{stats['passed']}/{stats['total']} ({pct:.0f}%)\033[0m")

    total_duration = sum(r["duration_ms"] for r in all_results)
    print(f"\nTotal time: {total_duration / 1000:.1f}s")
    print(f"{'='*60}")

    # Save results
    if args.save:
        RESULTS_DIR.mkdir(exist_ok=True)
        slug = model.replace("/", "_").replace(":", "_")
        filename = f"{datetime.now().strftime('%Y%m%d_%H%M%S')}_{slug}.json"
        output = {
            "timestamp": datetime.now().isoformat(),
            "backend": args.backend if not args.base_url else "custom",
            "base_url": base_url,
            "model": model,
            "total": total,
            "passed": total_passed,
            "failed": total_failed,
            "pass_rate": pass_rate,
            "categories": cat_stats,
            "results": all_results,
        }
        result_path = RESULTS_DIR / filename
        result_path.write_text(json.dumps(output, indent=2))
        print(f"\nResults saved to {result_path}")

    sys.exit(0 if total_failed == 0 else 1)


if __name__ == "__main__":
    main()
