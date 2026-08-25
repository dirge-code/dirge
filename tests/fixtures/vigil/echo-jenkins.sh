#!/bin/bash
# Simulate a Jenkins build completing and notifying vigil via harbinger.
echo '{"engine":"jenkins","job":"echo-job","build":42,"status":"SUCCESS"}' | nc -w1 127.0.0.1 9092
echo "jenkins echo sent (port 9092)"
