"""fifty-odd stdlib packages — thousands of code objects, each seen once

the case the `DISABLE` story is least able to help with. a code object that runs
once still costs its first `PY_START`, and a module body's lines are executed
exactly once, so a program whose time goes on importing is a program where every
event is a first event

it is also the shape of the thing most python programs spend their first second
doing, which is why it is here rather than only the loops
"""

# `sys` and `time` first, out of alphabetical order on purpose: the clock has to
# start before the imports being measured, and they are the work
import sys
import time

STARTED = time.perf_counter()

import argparse
import asyncio
import base64
import bisect
import calendar
import concurrent.futures
import csv
import dataclasses
import datetime
import decimal
import difflib
import email.parser
import fractions
import ftplib
import gzip
import hashlib
import html.parser
import http.client
import imaplib
import inspect
import ipaddress
import json
import logging.config
import mimetypes
import multiprocessing
import pathlib
import pdb
import pickle
import pprint
import queue
import random
import re
import secrets
import shutil
import smtplib
import socketserver
import sqlite3
import ssl
import statistics
import string
import subprocess
import tarfile
import tempfile
import textwrap
import threading
import tokenize
import typing
import unittest
import urllib.request
import uuid
import wave
import xml.etree.ElementTree
import xmlrpc.client
import zipfile
import zoneinfo

ELAPSED = time.perf_counter() - STARTED

# the exit code is the proof the imports really happened, so a run that failed
# to import something is not counted as a fast one
LOADED = len([name for name in dir() if not name.startswith("_")])

print(f"bpd-bench {ELAPSED * 1_000_000:.0f}", flush=True)
sys.exit(0 if LOADED >= 55 else 1)
