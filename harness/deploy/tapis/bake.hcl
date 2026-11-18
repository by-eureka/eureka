variable "BEAM_REV" {}
variable "IMAGE" {}
target "sdk" {
  context = "."
  dockerfile = "harness/deploy/agent.Dockerfile"
  args = { BEAM_REV = BEAM_REV }
  platforms = ["linux/amd64"]
}
target "pod" {
  context = "harness/deploy/tapis"
  contexts = { sdk = "target:sdk" }
  platforms = ["linux/amd64"]
  tags = [IMAGE]
}
