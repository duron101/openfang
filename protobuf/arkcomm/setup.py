from setuptools import setup, find_packages

setup(
    name="arkcomm",
    version="1.0.0",
    description="ARK Matrix 2.0 - ZeroMQ通信模块",
    author="ARK Matrix Team",
    packages=find_packages(),
    install_requires=[
        "pyzmq>=25.0.0",
        "numpy>=1.24.0",
    ],
    python_requires=">=3.8",
)
