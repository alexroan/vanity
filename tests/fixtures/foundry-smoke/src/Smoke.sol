// SPDX-License-Identifier: MIT
pragma solidity ^0.8.20;

contract Smoke {
    uint256 public immutable answer;

    constructor(uint256 answer_) {
        answer = answer_;
    }
}
