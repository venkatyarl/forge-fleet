"""Train a tiny task classifier (router model) from the router dataset.
Uses scikit-learn for a fast, <1ms inference model."""
import json, os, pickle
from collections import Counter

DATASET = os.path.expanduser("~/.forgefleet/training_data/router-dataset.jsonl")
MODEL_OUT = os.path.expanduser("~/.forgefleet/models/router-v1/")

# Load data
examples = []
with open(DATASET) as f:
    for line in f:
        ex = json.loads(line.strip())
        examples.append((ex["input"], ex["label"]))

print(f"Loaded {len(examples)} examples")
print(f"Distribution: {dict(Counter(l for _,l in examples))}")

# Train with TF-IDF + SVM (tiny, fast, effective for classification)
from sklearn.feature_extraction.text import TfidfVectorizer
from sklearn.svm import LinearSVC
from sklearn.pipeline import Pipeline
from sklearn.model_selection import cross_val_score

texts, labels = zip(*examples)

pipeline = Pipeline([
    ('tfidf', TfidfVectorizer(max_features=5000, ngram_range=(1,2), sublinear_tf=True)),
    ('clf', LinearSVC(max_iter=10000, C=1.0))
])

# Cross-validate
scores = cross_val_score(pipeline, texts, labels, cv=5, scoring='accuracy')
print(f"\nCross-validation accuracy: {scores.mean():.3f} (+/- {scores.std():.3f})")

# Train on full dataset
pipeline.fit(texts, labels)

# Save model
os.makedirs(MODEL_OUT, exist_ok=True)
with open(os.path.join(MODEL_OUT, "router.pkl"), "wb") as f:
    pickle.dump(pipeline, f)

# Test inference speed
import time
test_inputs = ["fix the auth bug", "ssh into marcus", "what is RAG?", "review this PR", "hi"]
start = time.perf_counter()
for _ in range(1000):
    for t in test_inputs:
        pipeline.predict([t])
elapsed = time.perf_counter() - start
per_call_us = (elapsed / 5000) * 1_000_000

print(f"\nInference speed: {per_call_us:.1f} microseconds per prediction")
print(f"Model saved to {MODEL_OUT}")

# Test predictions
print("\nSample predictions:")
for t in test_inputs:
    pred = pipeline.predict([t])[0]
    print(f"  '{t}' → {pred}")

# Also test some fleet-specific prompts
fleet_tests = [
    "deploy the new version to all computers",
    "write a function that parses JSON",
    "compare Qwen3 vs Llama 4",
    "check the security of this endpoint",
    "list files in the current directory",
    "research the latest MCP tools",
    "create a new React component for the dashboard",
    "restart llama-server on sophie",
]
print("\nFleet-specific predictions:")
for t in fleet_tests:
    pred = pipeline.predict([t])[0]
    print(f"  '{t}' → {pred}")

print(f"\nRouter model v1 ready! Size: {os.path.getsize(os.path.join(MODEL_OUT, 'router.pkl'))/1024:.1f} KB")
