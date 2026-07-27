// SPDX-License-Identifier: MIT
pragma solidity ^0.8.20;

contract Smoke {
    uint256 public immutable answer;
    address public immutable owner;

    constructor(uint256 answer_, address owner_) {
        answer = answer_;
        owner = owner_;
    }
}
