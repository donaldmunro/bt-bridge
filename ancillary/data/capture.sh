#!/bin/bash

tshark -i enp42s0f1u1c2 -f "host 192.168.0.237 or port 35100" -c 500000 -w capture.pcap
