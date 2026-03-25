from setuptools import setup, find_packages

setup(
    name="forgefleet",
    version="0.1.0",
    packages=find_packages(where="src"),
    package_dir={"": "."},
    entry_points={
        "console_scripts": [
            "forgefleet=cli:main",
        ],
    },
    install_requires=[
        "strands-agents",
        "strands-agents-tools",
    ],
    extras_require={
        "full": [
            "aider-chat",
            "cocoindex-code",
        ],
    },
    python_requires=">=3.10",
)
