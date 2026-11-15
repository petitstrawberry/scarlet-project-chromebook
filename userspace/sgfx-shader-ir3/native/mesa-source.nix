# Shared immutable source for host tests and the Scarlet runtime compiler.
{ pkgs }:
pkgs.fetchFromGitHub {
    owner = "chaotic-cx";
    repo = "mesa-mirror";
    rev = "3f1b217baffffa00cb8f53e158713a33e1bd4632";
    hash = "sha256-17NiUjgIP+uqOF2qC8kkBywmN3XdBJM1iUlrBRmuqM0=";
  }
