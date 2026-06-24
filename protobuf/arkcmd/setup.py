from setuptools import setup, find_packages

setup(
    name="arkcmd",
    version="1.0.0",
    description="ARK Matrix 2.0 - 指令生成模块",
    author="ARK Matrix Team",
    packages=find_packages(),
    install_requires=[
        "protobuf>=5.0.0",
        "grpcio>=1.60.0",
        "pyzmq>=25.0.0",
        "numpy>=1.24.0",
    ],
    python_requires=">=3.8",
)
